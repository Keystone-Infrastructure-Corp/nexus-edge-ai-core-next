//! ONVIF imaging service client (`ver20/imaging`).
//!
//! Phase 7.6.2. Reads and writes the image-pipeline settings of
//! a video source — brightness / contrast / saturation /
//! sharpness, backlight compensation, wide dynamic range, the
//! exposure block (auto vs manual exposure-time / gain / iris),
//! white balance, the IR-cut filter, defog, noise reduction and
//! image stabilization — plus the focus-motor controls
//! ([`focus_continuous_move`] / [`focus_absolute_move`] /
//! [`focus_stop`]). [`get_options`] reports the value ranges the
//! operator UI clamps its sliders to.
//!
//! Every request funnels through [`onvif_soap::IMAGING`]. The
//! `ImagingSettings20` tree is the most deeply nested ONVIF
//! payload we touch, so the parser keys each leaf on the nearest
//! ancestor *section* (e.g. a `Mode` under `Exposure` vs a `Mode`
//! under `WhiteBalance`) rather than on the leaf name alone.
//!
//! The public operations are the imaging-control surface consumed
//! by the operator admin-passthrough routes added in Phase 7.6.3
//! (`crate::device_control`).

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use super::onvif_soap::{local_name, xml_escape, IMAGING};

/// A `mode + optional level` control (BacklightCompensation,
/// WideDynamicRange, Defog, ImageStabilization).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModeLevel {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<f32>,
}

/// The exposure sub-block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Exposure {
    /// `AUTO` or `MANUAL`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_exposure_time: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_exposure_time: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_iris: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_iris: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure_time: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iris: Option<f32>,
}

/// The white-balance sub-block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WhiteBalance {
    /// `AUTO` or `MANUAL`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cr_gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cb_gain: Option<f32>,
}

/// The focus configuration sub-block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FocusConfig {
    /// `AUTO` or `MANUAL`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_focus_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_speed: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub near_limit: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub far_limit: Option<f32>,
}

/// The full `ImagingSettings20` field set. Every field is
/// optional so the same struct serves read-back (only what the
/// camera reports is populated) and partial writes (only what the
/// operator changed is sent).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ImagingSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contrast: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_saturation: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharpness: Option<f32>,
    /// `ON`, `OFF` or `AUTO`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ir_cut_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backlight_compensation: Option<ModeLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wide_dynamic_range: Option<ModeLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure: Option<Exposure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white_balance: Option<WhiteBalance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<FocusConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defog: Option<ModeLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noise_reduction: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_stabilization: Option<ModeLevel>,
}

/// A `[min, max]` option range.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct FloatRange {
    pub min: f32,
    pub max: f32,
}

/// The value ranges advertised by `GetOptions` for clamping the
/// operator UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ImagingOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<FloatRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contrast: Option<FloatRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_saturation: Option<FloatRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharpness: Option<FloatRange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ir_cut_filter_modes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Settings read / write
// ---------------------------------------------------------------------------

/// Read the current imaging settings for a video source.
pub async fn get_imaging_settings(
    endpoint: &str,
    username: &str,
    password: &str,
    video_source_token: &str,
) -> Result<ImagingSettings, String> {
    let body = format!(
        "<timg:GetImagingSettings><timg:VideoSourceToken>{}</timg:VideoSourceToken></timg:GetImagingSettings>",
        xml_escape(video_source_token)
    );
    let resp = IMAGING
        .call(endpoint, username, password, "GetImagingSettings", &body)
        .await?;
    Ok(parse_imaging_settings(&resp))
}

/// Write imaging settings. Only the `Some` fields are emitted, so
/// the operator can change one slider without disturbing the rest.
pub async fn set_imaging_settings(
    endpoint: &str,
    username: &str,
    password: &str,
    video_source_token: &str,
    settings: &ImagingSettings,
    force_persistence: bool,
) -> Result<(), String> {
    let body = set_imaging_settings_body(video_source_token, settings, force_persistence);
    IMAGING
        .call(endpoint, username, password, "SetImagingSettings", &body)
        .await
        .map(|_| ())
}

/// Read the option ranges for a video source.
pub async fn get_options(
    endpoint: &str,
    username: &str,
    password: &str,
    video_source_token: &str,
) -> Result<ImagingOptions, String> {
    let body = format!(
        "<timg:GetOptions><timg:VideoSourceToken>{}</timg:VideoSourceToken></timg:GetOptions>",
        xml_escape(video_source_token)
    );
    let resp = IMAGING
        .call(endpoint, username, password, "GetOptions", &body)
        .await?;
    Ok(parse_options(&resp))
}

// ---------------------------------------------------------------------------
// Focus motor
// ---------------------------------------------------------------------------

/// Drive the focus motor continuously at `speed` until stopped.
pub async fn focus_continuous_move(
    endpoint: &str,
    username: &str,
    password: &str,
    video_source_token: &str,
    speed: f32,
) -> Result<(), String> {
    let body = format!(
        "<timg:Move><timg:VideoSourceToken>{}</timg:VideoSourceToken><timg:Focus><tt:Continuous><tt:Speed>{}</tt:Speed></tt:Continuous></timg:Focus></timg:Move>",
        xml_escape(video_source_token),
        speed,
    );
    IMAGING
        .call(endpoint, username, password, "Move", &body)
        .await
        .map(|_| ())
}

/// Drive the focus motor to an absolute position.
pub async fn focus_absolute_move(
    endpoint: &str,
    username: &str,
    password: &str,
    video_source_token: &str,
    position: f32,
    speed: Option<f32>,
) -> Result<(), String> {
    let speed_xml = speed
        .map(|s| format!("<tt:Speed>{s}</tt:Speed>"))
        .unwrap_or_default();
    let body = format!(
        "<timg:Move><timg:VideoSourceToken>{}</timg:VideoSourceToken><timg:Focus><tt:Absolute><tt:Position>{}</tt:Position>{}</tt:Absolute></timg:Focus></timg:Move>",
        xml_escape(video_source_token),
        position,
        speed_xml,
    );
    IMAGING
        .call(endpoint, username, password, "Move", &body)
        .await
        .map(|_| ())
}

/// Stop the focus motor.
pub async fn focus_stop(
    endpoint: &str,
    username: &str,
    password: &str,
    video_source_token: &str,
) -> Result<(), String> {
    let body = format!(
        "<timg:Stop><timg:VideoSourceToken>{}</timg:VideoSourceToken></timg:Stop>",
        xml_escape(video_source_token)
    );
    IMAGING
        .call(endpoint, username, password, "Stop", &body)
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Request-body builder (pure)
// ---------------------------------------------------------------------------

fn tag_f32(local: &str, v: Option<f32>) -> String {
    v.map(|x| format!("<tt:{local}>{x}</tt:{local}>"))
        .unwrap_or_default()
}

fn tag_str(local: &str, v: &Option<String>) -> String {
    v.as_ref()
        .map(|s| format!("<tt:{local}>{}</tt:{local}>", xml_escape(s)))
        .unwrap_or_default()
}

fn mode_level_xml(container: &str, ml: &ModeLevel) -> String {
    if ml.mode.is_none() && ml.level.is_none() {
        return String::new();
    }
    format!(
        "<tt:{container}>{}{}</tt:{container}>",
        tag_str("Mode", &ml.mode),
        tag_f32("Level", ml.level),
    )
}

fn exposure_xml(e: &Exposure) -> String {
    // ImagingSettings20 schema sequence for Exposure.
    let inner = format!(
        "{}{}{}{}{}{}{}{}{}{}{}",
        tag_str("Mode", &e.mode),
        tag_str("Priority", &e.priority),
        tag_f32("MinExposureTime", e.min_exposure_time),
        tag_f32("MaxExposureTime", e.max_exposure_time),
        tag_f32("MinGain", e.min_gain),
        tag_f32("MaxGain", e.max_gain),
        tag_f32("MinIris", e.min_iris),
        tag_f32("MaxIris", e.max_iris),
        tag_f32("ExposureTime", e.exposure_time),
        tag_f32("Gain", e.gain),
        tag_f32("Iris", e.iris),
    );
    if inner.is_empty() {
        String::new()
    } else {
        format!("<tt:Exposure>{inner}</tt:Exposure>")
    }
}

fn white_balance_xml(w: &WhiteBalance) -> String {
    let inner = format!(
        "{}{}{}",
        tag_str("Mode", &w.mode),
        tag_f32("CrGain", w.cr_gain),
        tag_f32("CbGain", w.cb_gain),
    );
    if inner.is_empty() {
        String::new()
    } else {
        format!("<tt:WhiteBalance>{inner}</tt:WhiteBalance>")
    }
}

fn focus_xml(f: &FocusConfig) -> String {
    let inner = format!(
        "{}{}{}{}",
        tag_str("AutoFocusMode", &f.auto_focus_mode),
        tag_f32("DefaultSpeed", f.default_speed),
        tag_f32("NearLimit", f.near_limit),
        tag_f32("FarLimit", f.far_limit),
    );
    if inner.is_empty() {
        String::new()
    } else {
        format!("<tt:Focus>{inner}</tt:Focus>")
    }
}

fn extension_xml(s: &ImagingSettings) -> String {
    let image_stab = s
        .image_stabilization
        .as_ref()
        .map(|ml| mode_level_xml("ImageStabilization", ml))
        .unwrap_or_default();
    let defog = s
        .defog
        .as_ref()
        .map(|ml| mode_level_xml("Defog", ml))
        .unwrap_or_default();
    let noise = s
        .noise_reduction
        .map(|lvl| format!("<tt:NoiseReduction><tt:Level>{lvl}</tt:Level></tt:NoiseReduction>"))
        .unwrap_or_default();
    let inner = format!("{image_stab}{defog}{noise}");
    if inner.is_empty() {
        String::new()
    } else {
        format!("<tt:Extension>{inner}</tt:Extension>")
    }
}

fn set_imaging_settings_body(
    video_source_token: &str,
    s: &ImagingSettings,
    force_persistence: bool,
) -> String {
    // Children emitted in ImagingSettings20 schema sequence.
    let settings = format!(
        "{}{}{}{}{}{}{}{}{}{}",
        s.backlight_compensation
            .as_ref()
            .map(|ml| mode_level_xml("BacklightCompensation", ml))
            .unwrap_or_default(),
        tag_f32("Brightness", s.brightness),
        tag_f32("ColorSaturation", s.color_saturation),
        tag_f32("Contrast", s.contrast),
        s.exposure.as_ref().map(exposure_xml).unwrap_or_default(),
        s.focus.as_ref().map(focus_xml).unwrap_or_default(),
        tag_str("IrCutFilter", &s.ir_cut_filter),
        tag_f32("Sharpness", s.sharpness),
        s.wide_dynamic_range
            .as_ref()
            .map(|ml| mode_level_xml("WideDynamicRange", ml))
            .unwrap_or_default(),
        s.white_balance
            .as_ref()
            .map(white_balance_xml)
            .unwrap_or_default(),
    );
    format!(
        "<timg:SetImagingSettings><timg:VideoSourceToken>{}</timg:VideoSourceToken><timg:ImagingSettings>{}{}</timg:ImagingSettings><timg:ForcePersistence>{}</timg:ForcePersistence></timg:SetImagingSettings>",
        xml_escape(video_source_token),
        settings,
        extension_xml(s),
        force_persistence,
    )
}

// ---------------------------------------------------------------------------
// Response parsers (pure)
// ---------------------------------------------------------------------------

const SETTING_SECTIONS: &[&str] = &[
    "BacklightCompensation",
    "WideDynamicRange",
    "Exposure",
    "WhiteBalance",
    "Focus",
    "Defog",
    "NoiseReduction",
    "ImageStabilization",
];

/// The nearest enclosing settings section in `stack`, if any.
fn current_section(stack: &[String], sections: &[&'static str]) -> Option<&'static str> {
    stack
        .iter()
        .rev()
        .find_map(|s| sections.iter().copied().find(|sec| *sec == s.as_str()))
}

fn parse_imaging_settings(body: &str) -> ImagingSettings {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut out = ImagingSettings::default();
    let mut capture: Option<String> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(&e.name());
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
                        // Section excludes the leaf itself (still on stack top).
                        let section = current_section(
                            &stack[..stack.len().saturating_sub(1)],
                            SETTING_SECTIONS,
                        );
                        apply_setting_leaf(&mut out, section, &n, &v);
                    }
                }
                capture = None;
                stack.pop();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn apply_setting_leaf(out: &mut ImagingSettings, section: Option<&str>, leaf: &str, v: &str) {
    let f = || v.parse::<f32>().ok();
    match section {
        None => match leaf {
            "Brightness" => out.brightness = f(),
            "Contrast" => out.contrast = f(),
            "ColorSaturation" => out.color_saturation = f(),
            "Sharpness" => out.sharpness = f(),
            "IrCutFilter" => out.ir_cut_filter = Some(v.to_string()),
            _ => {}
        },
        Some("BacklightCompensation") => {
            let ml = out
                .backlight_compensation
                .get_or_insert_with(ModeLevel::default);
            apply_mode_level(ml, leaf, v);
        }
        Some("WideDynamicRange") => {
            let ml = out
                .wide_dynamic_range
                .get_or_insert_with(ModeLevel::default);
            apply_mode_level(ml, leaf, v);
        }
        Some("Defog") => {
            let ml = out.defog.get_or_insert_with(ModeLevel::default);
            apply_mode_level(ml, leaf, v);
        }
        Some("ImageStabilization") => {
            let ml = out
                .image_stabilization
                .get_or_insert_with(ModeLevel::default);
            apply_mode_level(ml, leaf, v);
        }
        Some("NoiseReduction") if leaf == "Level" => {
            out.noise_reduction = f();
        }
        Some("Exposure") => {
            let e = out.exposure.get_or_insert_with(Exposure::default);
            match leaf {
                "Mode" => e.mode = Some(v.to_string()),
                "Priority" => e.priority = Some(v.to_string()),
                "MinExposureTime" => e.min_exposure_time = f(),
                "MaxExposureTime" => e.max_exposure_time = f(),
                "MinGain" => e.min_gain = f(),
                "MaxGain" => e.max_gain = f(),
                "MinIris" => e.min_iris = f(),
                "MaxIris" => e.max_iris = f(),
                "ExposureTime" => e.exposure_time = f(),
                "Gain" => e.gain = f(),
                "Iris" => e.iris = f(),
                _ => {}
            }
        }
        Some("WhiteBalance") => {
            let w = out.white_balance.get_or_insert_with(WhiteBalance::default);
            match leaf {
                "Mode" => w.mode = Some(v.to_string()),
                "CrGain" => w.cr_gain = f(),
                "CbGain" => w.cb_gain = f(),
                _ => {}
            }
        }
        Some("Focus") => {
            let fc = out.focus.get_or_insert_with(FocusConfig::default);
            match leaf {
                "AutoFocusMode" => fc.auto_focus_mode = Some(v.to_string()),
                "DefaultSpeed" => fc.default_speed = f(),
                "NearLimit" => fc.near_limit = f(),
                "FarLimit" => fc.far_limit = f(),
                _ => {}
            }
        }
        _ => {}
    }
}

fn apply_mode_level(ml: &mut ModeLevel, leaf: &str, v: &str) {
    match leaf {
        "Mode" => ml.mode = Some(v.to_string()),
        "Level" => ml.level = v.parse::<f32>().ok(),
        _ => {}
    }
}

const OPTION_SECTIONS: &[&str] = &["Brightness", "Contrast", "ColorSaturation", "Sharpness"];

fn parse_options(body: &str) -> ImagingOptions {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut out = ImagingOptions::default();
    let mut capture: Option<String> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(&e.name());
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
                        if n == "IrCutFilterModes" {
                            out.ir_cut_filter_modes.push(v.clone());
                        } else if n == "Min" || n == "Max" {
                            let section = current_section(
                                &stack[..stack.len().saturating_sub(1)],
                                OPTION_SECTIONS,
                            );
                            if let (Some(sec), Ok(val)) = (section, v.parse::<f32>()) {
                                apply_option_bound(&mut out, sec, &n, val);
                            }
                        }
                    }
                }
                capture = None;
                stack.pop();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn apply_option_bound(out: &mut ImagingOptions, section: &str, bound: &str, val: f32) {
    let slot = match section {
        "Brightness" => &mut out.brightness,
        "Contrast" => &mut out.contrast,
        "ColorSaturation" => &mut out.color_saturation,
        "Sharpness" => &mut out.sharpness,
        _ => return,
    };
    let r = slot.get_or_insert_with(FloatRange::default);
    match bound {
        "Min" => r.min = val,
        "Max" => r.max = val,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(inner: &str) -> String {
        format!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:timg="http://www.onvif.org/ver20/imaging/wsdl"><s:Body>{inner}</s:Body></s:Envelope>"#
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
    fn parses_full_imaging_settings() {
        let body = wrap(
            r#"<timg:GetImagingSettingsResponse><timg:ImagingSettings>
                <tt:BacklightCompensation><tt:Mode>OFF</tt:Mode><tt:Level>0</tt:Level></tt:BacklightCompensation>
                <tt:Brightness>50</tt:Brightness>
                <tt:ColorSaturation>60</tt:ColorSaturation>
                <tt:Contrast>55</tt:Contrast>
                <tt:Exposure>
                    <tt:Mode>AUTO</tt:Mode>
                    <tt:MinExposureTime>10</tt:MinExposureTime>
                    <tt:MaxExposureTime>40000</tt:MaxExposureTime>
                    <tt:Gain>12</tt:Gain>
                </tt:Exposure>
                <tt:Focus><tt:AutoFocusMode>AUTO</tt:AutoFocusMode><tt:NearLimit>0.1</tt:NearLimit></tt:Focus>
                <tt:IrCutFilter>AUTO</tt:IrCutFilter>
                <tt:Sharpness>33</tt:Sharpness>
                <tt:WideDynamicRange><tt:Mode>ON</tt:Mode><tt:Level>80</tt:Level></tt:WideDynamicRange>
                <tt:WhiteBalance><tt:Mode>AUTO</tt:Mode><tt:CrGain>1.5</tt:CrGain><tt:CbGain>2.5</tt:CbGain></tt:WhiteBalance>
                <tt:Extension>
                    <tt:ImageStabilization><tt:Mode>ON</tt:Mode></tt:ImageStabilization>
                    <tt:Defog><tt:Mode>AUTO</tt:Mode><tt:Level>20</tt:Level></tt:Defog>
                    <tt:NoiseReduction><tt:Level>15</tt:Level></tt:NoiseReduction>
                </tt:Extension>
            </timg:ImagingSettings></timg:GetImagingSettingsResponse>"#,
        );
        let s = parse_imaging_settings(&body);
        assert_eq!(s.brightness, Some(50.0));
        assert_eq!(s.color_saturation, Some(60.0));
        assert_eq!(s.contrast, Some(55.0));
        assert_eq!(s.sharpness, Some(33.0));
        assert_eq!(s.ir_cut_filter.as_deref(), Some("AUTO"));
        assert_eq!(
            s.backlight_compensation,
            Some(ModeLevel {
                mode: Some("OFF".into()),
                level: Some(0.0)
            })
        );
        assert_eq!(
            s.wide_dynamic_range,
            Some(ModeLevel {
                mode: Some("ON".into()),
                level: Some(80.0)
            })
        );
        let exp = s.exposure.expect("exposure");
        assert_eq!(exp.mode.as_deref(), Some("AUTO"));
        assert_eq!(exp.min_exposure_time, Some(10.0));
        assert_eq!(exp.max_exposure_time, Some(40000.0));
        assert_eq!(exp.gain, Some(12.0));
        let wb = s.white_balance.expect("wb");
        assert_eq!(wb.mode.as_deref(), Some("AUTO"));
        assert_eq!(wb.cr_gain, Some(1.5));
        assert_eq!(wb.cb_gain, Some(2.5));
        let focus = s.focus.expect("focus");
        assert_eq!(focus.auto_focus_mode.as_deref(), Some("AUTO"));
        assert_eq!(focus.near_limit, Some(0.1));
        assert_eq!(
            s.image_stabilization,
            Some(ModeLevel {
                mode: Some("ON".into()),
                level: None
            })
        );
        assert_eq!(
            s.defog,
            Some(ModeLevel {
                mode: Some("AUTO".into()),
                level: Some(20.0)
            })
        );
        assert_eq!(s.noise_reduction, Some(15.0));
    }

    #[test]
    fn set_imaging_settings_body_emits_only_present_fields() {
        let s = ImagingSettings {
            brightness: Some(70.0),
            exposure: Some(Exposure {
                mode: Some("MANUAL".into()),
                exposure_time: Some(2000.0),
                ..Exposure::default()
            }),
            ..ImagingSettings::default()
        };
        let body = set_imaging_settings_body("VST_1", &s, true);
        assert_well_formed(&wrap(&body));
        assert!(body.contains("<tt:Brightness>70</tt:Brightness>"), "{body}");
        assert!(body.contains("<tt:Exposure><tt:Mode>MANUAL</tt:Mode><tt:ExposureTime>2000</tt:ExposureTime></tt:Exposure>"), "{body}");
        assert!(
            body.contains("<timg:ForcePersistence>true</timg:ForcePersistence>"),
            "{body}"
        );
        // Untouched fields must be absent.
        assert!(!body.contains("Contrast"), "{body}");
        assert!(!body.contains("WhiteBalance"), "{body}");
    }

    #[test]
    fn parses_imaging_options_ranges_and_modes() {
        let body = wrap(
            r#"<timg:GetOptionsResponse><timg:ImagingOptions>
                <tt:Brightness><tt:Min>0</tt:Min><tt:Max>100</tt:Max></tt:Brightness>
                <tt:Contrast><tt:Min>0</tt:Min><tt:Max>100</tt:Max></tt:Contrast>
                <tt:ColorSaturation><tt:Min>0</tt:Min><tt:Max>100</tt:Max></tt:ColorSaturation>
                <tt:Sharpness><tt:Min>0</tt:Min><tt:Max>15</tt:Max></tt:Sharpness>
                <tt:IrCutFilterModes>ON</tt:IrCutFilterModes>
                <tt:IrCutFilterModes>OFF</tt:IrCutFilterModes>
                <tt:IrCutFilterModes>AUTO</tt:IrCutFilterModes>
            </timg:ImagingOptions></timg:GetOptionsResponse>"#,
        );
        let o = parse_options(&body);
        assert_eq!(
            o.brightness,
            Some(FloatRange {
                min: 0.0,
                max: 100.0
            })
        );
        assert_eq!(
            o.sharpness,
            Some(FloatRange {
                min: 0.0,
                max: 15.0
            })
        );
        assert_eq!(o.ir_cut_filter_modes, vec!["ON", "OFF", "AUTO"]);
    }

    #[test]
    fn focus_move_bodies_are_well_formed() {
        let cont = "<timg:Move><timg:VideoSourceToken>V</timg:VideoSourceToken><timg:Focus><tt:Continuous><tt:Speed>0.5</tt:Speed></tt:Continuous></timg:Focus></timg:Move>";
        assert_well_formed(&wrap(cont));
    }
}
