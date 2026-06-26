//! ONVIF PTZ service client (`ver20/ptz`).
//!
//! Phase 7.6.2. Drives a pan/tilt/zoom camera through its ONVIF
//! PTZ service: jog (continuous), nudge (relative), go-to
//! (absolute), stop, the preset book (list / recall / store /
//! delete), the home position, and auxiliary commands (wiper /
//! washer / IR-lamp / heater). It also reads back the node
//! capability set ([`get_nodes`]) and the movement-space ranges
//! ([`get_configuration_options`]) the operator UI needs to
//! clamp its sliders.
//!
//! Every request funnels through [`onvif_soap::PTZ`], so the
//! transport, the WS-Security digest header, and the SOAP-fault
//! handling are shared with every other ONVIF service module.
//! The request-body builders and the response parsers are split
//! into named, side-effect-free functions so they can be unit
//! tested against recorded camera fixtures without a network.
//!
//! The public `async` operations are the camera-control surface
//! consumed by the operator admin-passthrough routes added in
//! Phase 7.6.3; until those routes land they have no in-crate
//! caller, hence the module-level `dead_code` allowance.
#![allow(dead_code)]

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use super::onvif_soap::{first_text, local_name, xml_escape, PTZ};

/// A pan/tilt/zoom triple. Used for velocities (continuous
/// move), translations (relative move), positions (absolute move
/// / preset position) and speeds. ONVIF's generic spaces
/// normalise pan/tilt to `-1.0..=1.0` and zoom to `0.0..=1.0`;
/// device-specific spaces may differ, which is what
/// [`get_configuration_options`] reports.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct PtzVector {
    pub pan: f32,
    pub tilt: f32,
    pub zoom: f32,
}

/// One stored PTZ preset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Preset {
    pub token: String,
    pub name: String,
    /// The saved position, when the camera reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<PtzVector>,
}

/// Live PTZ status (`GetStatus`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PtzStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<PtzVector>,
    /// Aggregate move status string (`IDLE` / `MOVING`), taken
    /// from the pan/tilt move-status field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utc_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A `[min, max]` movement range for one axis.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct PtzRange {
    pub min: f32,
    pub max: f32,
}

/// The absolute-position movement ranges the camera advertises,
/// extracted from `SupportedPTZSpaces` / `PTZConfigurationOptions`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PtzSpaces {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pan: Option<PtzRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tilt: Option<PtzRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zoom: Option<PtzRange>,
}

/// One PTZ node (a movable head). Most cameras expose exactly
/// one. The fields are the subset the operator UI needs to decide
/// what controls to render.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PtzNode {
    pub token: String,
    pub name: String,
    pub home_supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_presets: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aux_commands: Vec<String>,
}

// ---------------------------------------------------------------------------
// Movement
// ---------------------------------------------------------------------------

/// Jog the camera at the given velocity until stopped (or the
/// optional `timeout` elapses).
pub async fn continuous_move(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
    velocity: PtzVector,
    timeout_secs: Option<f32>,
) -> Result<(), String> {
    let body = continuous_move_body(profile_token, &velocity, timeout_secs);
    PTZ.call(endpoint, username, password, "ContinuousMove", &body)
        .await
        .map(|_| ())
}

/// Nudge the camera by a relative translation.
pub async fn relative_move(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
    translation: PtzVector,
    speed: Option<PtzVector>,
) -> Result<(), String> {
    let body = move_with_speed_body(
        "RelativeMove",
        "Translation",
        profile_token,
        &translation,
        speed.as_ref(),
    );
    PTZ.call(endpoint, username, password, "RelativeMove", &body)
        .await
        .map(|_| ())
}

/// Drive the camera to an absolute position.
pub async fn absolute_move(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
    position: PtzVector,
    speed: Option<PtzVector>,
) -> Result<(), String> {
    let body = move_with_speed_body(
        "AbsoluteMove",
        "Position",
        profile_token,
        &position,
        speed.as_ref(),
    );
    PTZ.call(endpoint, username, password, "AbsoluteMove", &body)
        .await
        .map(|_| ())
}

/// Stop pan/tilt and/or zoom motion.
pub async fn stop(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
    pan_tilt: bool,
    zoom: bool,
) -> Result<(), String> {
    let body = stop_body(profile_token, pan_tilt, zoom);
    PTZ.call(endpoint, username, password, "Stop", &body)
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

/// List the stored presets.
pub async fn get_presets(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
) -> Result<Vec<Preset>, String> {
    let body = format!(
        "<tptz:GetPresets><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetPresets>",
        xml_escape(profile_token)
    );
    let resp = PTZ
        .call(endpoint, username, password, "GetPresets", &body)
        .await?;
    Ok(parse_presets(&resp))
}

/// Recall a preset.
pub async fn goto_preset(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
    preset_token: &str,
    speed: Option<PtzVector>,
) -> Result<(), String> {
    let body = format!(
        "<tptz:GotoPreset><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetToken>{}</tptz:PresetToken>{}</tptz:GotoPreset>",
        xml_escape(profile_token),
        xml_escape(preset_token),
        speed.as_ref().map(speed_xml).unwrap_or_default(),
    );
    PTZ.call(endpoint, username, password, "GotoPreset", &body)
        .await
        .map(|_| ())
}

/// Store (create or overwrite) a preset. Returns the preset
/// token the camera assigned. When `preset_token` is supplied the
/// existing preset at that token is overwritten with the camera's
/// current position; otherwise a new preset is created.
pub async fn set_preset(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
    preset_name: Option<&str>,
    preset_token: Option<&str>,
) -> Result<String, String> {
    let body = set_preset_body(profile_token, preset_name, preset_token);
    let resp = PTZ
        .call(endpoint, username, password, "SetPreset", &body)
        .await?;
    first_text(&resp, "PresetToken")
        .or_else(|| preset_token.map(|t| t.to_string()))
        .ok_or_else(|| "SetPreset response missing PresetToken".to_string())
}

/// Delete a preset.
pub async fn remove_preset(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
    preset_token: &str,
) -> Result<(), String> {
    let body = format!(
        "<tptz:RemovePreset><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetToken>{}</tptz:PresetToken></tptz:RemovePreset>",
        xml_escape(profile_token),
        xml_escape(preset_token),
    );
    PTZ.call(endpoint, username, password, "RemovePreset", &body)
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Home position + auxiliary
// ---------------------------------------------------------------------------

/// Save the current position as the home position.
pub async fn set_home_position(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
) -> Result<(), String> {
    let body = format!(
        "<tptz:SetHomePosition><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:SetHomePosition>",
        xml_escape(profile_token)
    );
    PTZ.call(endpoint, username, password, "SetHomePosition", &body)
        .await
        .map(|_| ())
}

/// Recall the home position.
pub async fn goto_home_position(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
    speed: Option<PtzVector>,
) -> Result<(), String> {
    let body = format!(
        "<tptz:GotoHomePosition><tptz:ProfileToken>{}</tptz:ProfileToken>{}</tptz:GotoHomePosition>",
        xml_escape(profile_token),
        speed.as_ref().map(speed_xml).unwrap_or_default(),
    );
    PTZ.call(endpoint, username, password, "GotoHomePosition", &body)
        .await
        .map(|_| ())
}

/// Send an auxiliary command (e.g. `tt:Wiper|On`, `tt:Washer|On`,
/// `tt:IRLamp|On`). The exact tokens come from the node's
/// `AuxiliaryCommands` list ([`get_nodes`]).
pub async fn send_auxiliary_command(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
    aux_command: &str,
) -> Result<(), String> {
    let body = format!(
        "<tptz:SendAuxiliaryCommand><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:AuxiliaryData>{}</tptz:AuxiliaryData></tptz:SendAuxiliaryCommand>",
        xml_escape(profile_token),
        xml_escape(aux_command),
    );
    PTZ.call(endpoint, username, password, "SendAuxiliaryCommand", &body)
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Status + capability read-back
// ---------------------------------------------------------------------------

/// Read live PTZ status.
pub async fn get_status(
    endpoint: &str,
    username: &str,
    password: &str,
    profile_token: &str,
) -> Result<PtzStatus, String> {
    let body = format!(
        "<tptz:GetStatus><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetStatus>",
        xml_escape(profile_token)
    );
    let resp = PTZ
        .call(endpoint, username, password, "GetStatus", &body)
        .await?;
    Ok(parse_status(&resp))
}

/// Read the PTZ nodes (movable heads) and their capabilities.
pub async fn get_nodes(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<Vec<PtzNode>, String> {
    let resp = PTZ
        .call(endpoint, username, password, "GetNodes", "<tptz:GetNodes/>")
        .await?;
    Ok(parse_nodes(&resp))
}

/// Read the movement-space ranges for a PTZ configuration.
pub async fn get_configuration_options(
    endpoint: &str,
    username: &str,
    password: &str,
    config_token: &str,
) -> Result<PtzSpaces, String> {
    let body = format!(
        "<tptz:GetConfigurationOptions><tptz:ConfigurationToken>{}</tptz:ConfigurationToken></tptz:GetConfigurationOptions>",
        xml_escape(config_token)
    );
    let resp = PTZ
        .call(
            endpoint,
            username,
            password,
            "GetConfigurationOptions",
            &body,
        )
        .await?;
    Ok(parse_spaces(&resp))
}

// ---------------------------------------------------------------------------
// Request-body builders (pure)
// ---------------------------------------------------------------------------

/// `<tptz:{wrapper}><tt:PanTilt .../><tt:Zoom .../></tptz:{wrapper}>`.
fn vector_xml(wrapper: &str, v: &PtzVector) -> String {
    format!(
        "<tptz:{w}><tt:PanTilt x=\"{pan}\" y=\"{tilt}\"/><tt:Zoom x=\"{zoom}\"/></tptz:{w}>",
        w = wrapper,
        pan = v.pan,
        tilt = v.tilt,
        zoom = v.zoom,
    )
}

fn speed_xml(v: &PtzVector) -> String {
    vector_xml("Speed", v)
}

fn continuous_move_body(
    profile_token: &str,
    velocity: &PtzVector,
    timeout_secs: Option<f32>,
) -> String {
    let timeout = timeout_secs
        .map(|s| format!("<tptz:Timeout>PT{s}S</tptz:Timeout>"))
        .unwrap_or_default();
    format!(
        "<tptz:ContinuousMove><tptz:ProfileToken>{}</tptz:ProfileToken>{}{}</tptz:ContinuousMove>",
        xml_escape(profile_token),
        vector_xml("Velocity", velocity),
        timeout,
    )
}

fn move_with_speed_body(
    op: &str,
    vector_tag: &str,
    profile_token: &str,
    vector: &PtzVector,
    speed: Option<&PtzVector>,
) -> String {
    format!(
        "<tptz:{op}><tptz:ProfileToken>{}</tptz:ProfileToken>{}{}</tptz:{op}>",
        xml_escape(profile_token),
        vector_xml(vector_tag, vector),
        speed.map(speed_xml).unwrap_or_default(),
        op = op,
    )
}

fn stop_body(profile_token: &str, pan_tilt: bool, zoom: bool) -> String {
    format!(
        "<tptz:Stop><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PanTilt>{}</tptz:PanTilt><tptz:Zoom>{}</tptz:Zoom></tptz:Stop>",
        xml_escape(profile_token),
        pan_tilt,
        zoom,
    )
}

fn set_preset_body(
    profile_token: &str,
    preset_name: Option<&str>,
    preset_token: Option<&str>,
) -> String {
    let name = preset_name
        .map(|n| format!("<tptz:PresetName>{}</tptz:PresetName>", xml_escape(n)))
        .unwrap_or_default();
    let token = preset_token
        .map(|t| format!("<tptz:PresetToken>{}</tptz:PresetToken>", xml_escape(t)))
        .unwrap_or_default();
    format!(
        "<tptz:SetPreset><tptz:ProfileToken>{}</tptz:ProfileToken>{}{}</tptz:SetPreset>",
        xml_escape(profile_token),
        name,
        token,
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

fn attr_f32(e: &BytesStart, key: &str) -> Option<f32> {
    attr_str(e, key).and_then(|v| v.trim().parse::<f32>().ok())
}

fn parse_presets(body: &str) -> Vec<Preset> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut cur: Option<Preset> = None;
    let mut pos: Option<PtzVector> = None;
    let mut in_position = false;
    let mut in_name = false;
    let mut name_acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e.name()).as_str() {
                "Preset" => {
                    cur = Some(Preset {
                        token: attr_str(&e, "token").unwrap_or_default(),
                        name: String::new(),
                        position: None,
                    });
                    pos = None;
                    in_position = false;
                }
                "PTZPosition" if cur.is_some() => {
                    in_position = true;
                    pos.get_or_insert(PtzVector {
                        pan: 0.0,
                        tilt: 0.0,
                        zoom: 0.0,
                    });
                }
                "Name" if cur.is_some() => {
                    in_name = true;
                    name_acc.clear();
                }
                "PanTilt" if in_position => {
                    let p = pos.get_or_insert(PtzVector {
                        pan: 0.0,
                        tilt: 0.0,
                        zoom: 0.0,
                    });
                    if let Some(x) = attr_f32(&e, "x") {
                        p.pan = x;
                    }
                    if let Some(y) = attr_f32(&e, "y") {
                        p.tilt = y;
                    }
                }
                "Zoom" if in_position => {
                    let p = pos.get_or_insert(PtzVector {
                        pan: 0.0,
                        tilt: 0.0,
                        zoom: 0.0,
                    });
                    if let Some(x) = attr_f32(&e, "x") {
                        p.zoom = x;
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_name => {
                if let Ok(s) = t.unescape() {
                    name_acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => match local_name(&e.name()).as_str() {
                "Name" if in_name => {
                    if let Some(c) = cur.as_mut() {
                        c.name = name_acc.trim().to_string();
                    }
                    in_name = false;
                }
                "PTZPosition" => in_position = false,
                "Preset" => {
                    if let Some(mut c) = cur.take() {
                        c.position = pos.take();
                        out.push(c);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn parse_status(body: &str) -> PtzStatus {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut status = PtzStatus::default();
    let mut pos: Option<PtzVector> = None;
    let mut in_position = false;
    let mut in_move_status = false;
    let mut capture_move = false;
    let mut move_acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e.name()).as_str() {
                "Position" => {
                    in_position = true;
                    pos.get_or_insert(PtzVector {
                        pan: 0.0,
                        tilt: 0.0,
                        zoom: 0.0,
                    });
                }
                "MoveStatus" => in_move_status = true,
                "PanTilt" if in_position => {
                    let p = pos.get_or_insert(PtzVector {
                        pan: 0.0,
                        tilt: 0.0,
                        zoom: 0.0,
                    });
                    if let Some(x) = attr_f32(&e, "x") {
                        p.pan = x;
                    }
                    if let Some(y) = attr_f32(&e, "y") {
                        p.tilt = y;
                    }
                }
                "Zoom" if in_position => {
                    let p = pos.get_or_insert(PtzVector {
                        pan: 0.0,
                        tilt: 0.0,
                        zoom: 0.0,
                    });
                    if let Some(x) = attr_f32(&e, "x") {
                        p.zoom = x;
                    }
                }
                "PanTilt" if in_move_status => {
                    capture_move = true;
                    move_acc.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if capture_move => {
                if let Ok(s) = t.unescape() {
                    move_acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => match local_name(&e.name()).as_str() {
                "Position" => in_position = false,
                "MoveStatus" => in_move_status = false,
                "PanTilt" if capture_move => {
                    if status.move_status.is_none() && !move_acc.trim().is_empty() {
                        status.move_status = Some(move_acc.trim().to_string());
                    }
                    capture_move = false;
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    status.position = pos;
    status.utc_time = first_text(body, "UtcTime");
    status.error = first_text(body, "Error");
    status
}

fn parse_nodes(body: &str) -> Vec<PtzNode> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut out = Vec::new();
    let mut cur: Option<PtzNode> = None;
    let mut capture: Option<&'static str> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(&e.name());
                let parent_node = stack.last().map(|s| s == "PTZNode").unwrap_or(false);
                match n.as_str() {
                    "PTZNode" => {
                        cur = Some(PtzNode {
                            token: attr_str(&e, "token").unwrap_or_default(),
                            name: String::new(),
                            home_supported: false,
                            max_presets: None,
                            aux_commands: Vec::new(),
                        });
                    }
                    "Name" if parent_node => {
                        capture = Some("Name");
                        acc.clear();
                    }
                    "HomeSupported" if parent_node => {
                        capture = Some("HomeSupported");
                        acc.clear();
                    }
                    "MaximumNumberOfPresets" if parent_node => {
                        capture = Some("MaximumNumberOfPresets");
                        acc.clear();
                    }
                    "AuxiliaryCommands" if parent_node => {
                        capture = Some("AuxiliaryCommands");
                        acc.clear();
                    }
                    _ => {}
                }
                stack.push(n);
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                stack.pop();
                let n = local_name(&e.name());
                if let Some(field) = capture {
                    if field == n {
                        if let Some(c) = cur.as_mut() {
                            let val = acc.trim().to_string();
                            match field {
                                "Name" => c.name = val,
                                "HomeSupported" => {
                                    c.home_supported = val.eq_ignore_ascii_case("true")
                                }
                                "MaximumNumberOfPresets" => c.max_presets = val.parse().ok(),
                                "AuxiliaryCommands" if !val.is_empty() => c.aux_commands.push(val),
                                _ => {}
                            }
                        }
                        capture = None;
                    }
                }
                if n == "PTZNode" {
                    if let Some(c) = cur.take() {
                        out.push(c);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Extract pan/tilt/zoom absolute-position ranges from any body
/// carrying `AbsolutePanTiltPositionSpace` / `AbsoluteZoomPositionSpace`
/// (both `SupportedPTZSpaces` and `PTZConfigurationOptions` use them).
fn parse_spaces(body: &str) -> PtzSpaces {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut spaces = PtzSpaces::default();
    // Which space we're inside: 0=none, 1=pan/tilt position, 2=zoom position.
    let mut space = 0u8;
    // Which range we're inside: 0=none, b'x', b'y'.
    let mut range = 0u8;
    let mut field: Option<&'static str> = None; // "Min" | "Max"
    let mut acc = String::new();
    // pending values per axis
    let mut pan = PtzRange::default();
    let mut tilt = PtzRange::default();
    let mut zoom = PtzRange::default();
    let mut have_pan = false;
    let mut have_zoom = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(&e.name()).as_str() {
                "AbsolutePanTiltPositionSpace" => space = 1,
                "AbsoluteZoomPositionSpace" => space = 2,
                "XRange" => range = b'x',
                "YRange" => range = b'y',
                "Min" => {
                    field = Some("Min");
                    acc.clear();
                }
                "Max" => {
                    field = Some("Max");
                    acc.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if field.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => match local_name(&e.name()).as_str() {
                "AbsolutePanTiltPositionSpace" => space = 0,
                "AbsoluteZoomPositionSpace" => space = 0,
                "XRange" | "YRange" => range = 0,
                "Min" | "Max" => {
                    if let (Some(f), Ok(v)) = (field.take(), acc.trim().parse::<f32>()) {
                        match (space, range, f) {
                            (1, b'x', "Min") => {
                                pan.min = v;
                                have_pan = true;
                            }
                            (1, b'x', "Max") => {
                                pan.max = v;
                                have_pan = true;
                            }
                            (1, b'y', "Min") => tilt.min = v,
                            (1, b'y', "Max") => tilt.max = v,
                            (2, b'x', "Min") => {
                                zoom.min = v;
                                have_zoom = true;
                            }
                            (2, b'x', "Max") => {
                                zoom.max = v;
                                have_zoom = true;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    if have_pan {
        spaces.pan = Some(pan);
        spaces.tilt = Some(tilt);
    }
    if have_zoom {
        spaces.zoom = Some(zoom);
    }
    spaces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(inner: &str) -> String {
        // Wrap a body fragment in a minimal envelope to validate
        // it is well-formed XML (the real envelope is added by
        // `onvif_soap::OnvifService::envelope`).
        format!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><s:Body>{inner}</s:Body></s:Envelope>"#
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
    fn continuous_move_body_carries_velocity_and_timeout() {
        let body = continuous_move_body(
            "Profile_1",
            &PtzVector {
                pan: -0.5,
                tilt: 0.25,
                zoom: 0.0,
            },
            Some(3.0),
        );
        assert_well_formed(&wrap(&body));
        assert!(body.contains("<tptz:ProfileToken>Profile_1</tptz:ProfileToken>"));
        assert!(
            body.contains(r#"<tt:PanTilt x="-0.5" y="0.25"/>"#),
            "{body}"
        );
        assert!(body.contains("<tptz:Timeout>PT3S</tptz:Timeout>"), "{body}");
    }

    #[test]
    fn absolute_move_body_includes_position_and_speed() {
        let body = move_with_speed_body(
            "AbsoluteMove",
            "Position",
            "P1",
            &PtzVector {
                pan: 0.1,
                tilt: -0.2,
                zoom: 0.7,
            },
            Some(&PtzVector {
                pan: 1.0,
                tilt: 1.0,
                zoom: 1.0,
            }),
        );
        assert_well_formed(&wrap(&body));
        assert!(body.contains("<tptz:Position>"), "{body}");
        assert!(body.contains("<tptz:Speed>"), "{body}");
        assert!(body.contains(r#"<tt:Zoom x="0.7"/>"#), "{body}");
    }

    #[test]
    fn stop_body_sets_pan_tilt_and_zoom_flags() {
        let body = stop_body("P1", true, false);
        assert_well_formed(&wrap(&body));
        assert!(body.contains("<tptz:PanTilt>true</tptz:PanTilt>"), "{body}");
        assert!(body.contains("<tptz:Zoom>false</tptz:Zoom>"), "{body}");
    }

    #[test]
    fn set_preset_body_omits_optional_fields_when_absent() {
        let with_name = set_preset_body("P1", Some("Front Door"), None);
        assert_well_formed(&wrap(&with_name));
        assert!(with_name.contains("<tptz:PresetName>Front Door</tptz:PresetName>"));
        assert!(!with_name.contains("PresetToken"), "{with_name}");

        let overwrite = set_preset_body("P1", None, Some("3"));
        assert!(overwrite.contains("<tptz:PresetToken>3</tptz:PresetToken>"));
        assert!(!overwrite.contains("PresetName"), "{overwrite}");
    }

    #[test]
    fn parses_get_presets_response() {
        // Hikvision-style GetPresetsResponse with prefixed and
        // mixed namespaces; the parser keys on local names only.
        let body = wrap(
            r#"<tptz:GetPresetsResponse>
                <tptz:Preset token="1">
                    <tt:Name>Front Gate</tt:Name>
                    <tt:PTZPosition>
                        <tt:PanTilt x="0.1" y="-0.2"/>
                        <tt:Zoom x="0.5"/>
                    </tt:PTZPosition>
                </tptz:Preset>
                <tptz:Preset token="2">
                    <tt:Name>Parking</tt:Name>
                </tptz:Preset>
            </tptz:GetPresetsResponse>"#,
        );
        let presets = parse_presets(&body);
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].token, "1");
        assert_eq!(presets[0].name, "Front Gate");
        assert_eq!(
            presets[0].position,
            Some(PtzVector {
                pan: 0.1,
                tilt: -0.2,
                zoom: 0.5
            })
        );
        assert_eq!(presets[1].name, "Parking");
        assert_eq!(presets[1].position, None);
    }

    #[test]
    fn parses_get_status_response() {
        let body = wrap(
            r#"<tptz:GetStatusResponse><tptz:PTZStatus>
                <tt:Position>
                    <tt:PanTilt x="0.33" y="0.44"/>
                    <tt:Zoom x="0.0"/>
                </tt:Position>
                <tt:MoveStatus>
                    <tt:PanTilt>IDLE</tt:PanTilt>
                    <tt:Zoom>IDLE</tt:Zoom>
                </tt:MoveStatus>
                <tt:UtcTime>2024-05-01T12:34:56Z</tt:UtcTime>
            </tptz:PTZStatus></tptz:GetStatusResponse>"#,
        );
        let st = parse_status(&body);
        assert_eq!(
            st.position,
            Some(PtzVector {
                pan: 0.33,
                tilt: 0.44,
                zoom: 0.0
            })
        );
        assert_eq!(st.move_status.as_deref(), Some("IDLE"));
        assert_eq!(st.utc_time.as_deref(), Some("2024-05-01T12:34:56Z"));
    }

    #[test]
    fn parses_get_nodes_response() {
        let body = wrap(
            r#"<tptz:GetNodesResponse><tptz:PTZNode token="PTZNodeToken_1">
                <tt:Name>MainPTZNode</tt:Name>
                <tt:SupportedPTZSpaces>
                    <tt:AbsolutePanTiltPositionSpace>
                        <tt:URI>http://www.onvif.org/ver10/tptz/PanTiltSpaces/PositionGenericSpace</tt:URI>
                        <tt:XRange><tt:Min>-1.0</tt:Min><tt:Max>1.0</tt:Max></tt:XRange>
                        <tt:YRange><tt:Min>-1.0</tt:Min><tt:Max>1.0</tt:Max></tt:YRange>
                    </tt:AbsolutePanTiltPositionSpace>
                </tt:SupportedPTZSpaces>
                <tt:MaximumNumberOfPresets>300</tt:MaximumNumberOfPresets>
                <tt:HomeSupported>true</tt:HomeSupported>
                <tt:AuxiliaryCommands>tt:Wiper|On</tt:AuxiliaryCommands>
                <tt:AuxiliaryCommands>tt:IRLamp|On</tt:AuxiliaryCommands>
            </tptz:PTZNode></tptz:GetNodesResponse>"#,
        );
        let nodes = parse_nodes(&body);
        assert_eq!(nodes.len(), 1);
        let n = &nodes[0];
        assert_eq!(n.token, "PTZNodeToken_1");
        assert_eq!(n.name, "MainPTZNode");
        assert!(n.home_supported);
        assert_eq!(n.max_presets, Some(300));
        assert_eq!(n.aux_commands, vec!["tt:Wiper|On", "tt:IRLamp|On"]);
    }

    #[test]
    fn parses_configuration_options_spaces() {
        let body = wrap(
            r#"<tptz:GetConfigurationOptionsResponse><tptz:PTZConfigurationOptions>
                <tt:Spaces>
                    <tt:AbsolutePanTiltPositionSpace>
                        <tt:XRange><tt:Min>-180</tt:Min><tt:Max>180</tt:Max></tt:XRange>
                        <tt:YRange><tt:Min>-90</tt:Min><tt:Max>90</tt:Max></tt:YRange>
                    </tt:AbsolutePanTiltPositionSpace>
                    <tt:AbsoluteZoomPositionSpace>
                        <tt:XRange><tt:Min>0</tt:Min><tt:Max>1</tt:Max></tt:XRange>
                    </tt:AbsoluteZoomPositionSpace>
                </tt:Spaces>
            </tptz:PTZConfigurationOptions></tptz:GetConfigurationOptionsResponse>"#,
        );
        let s = parse_spaces(&body);
        assert_eq!(
            s.pan,
            Some(PtzRange {
                min: -180.0,
                max: 180.0
            })
        );
        assert_eq!(
            s.tilt,
            Some(PtzRange {
                min: -90.0,
                max: 90.0
            })
        );
        assert_eq!(s.zoom, Some(PtzRange { min: 0.0, max: 1.0 }));
    }

    #[test]
    fn parses_set_preset_response_token() {
        let body = wrap(
            r#"<tptz:SetPresetResponse><tptz:PresetToken>7</tptz:PresetToken></tptz:SetPresetResponse>"#,
        );
        assert_eq!(first_text(&body, "PresetToken").as_deref(), Some("7"));
    }
}
