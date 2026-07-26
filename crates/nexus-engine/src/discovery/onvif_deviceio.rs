//! ONVIF device-I/O + OSD client.
//!
//! Phase 7.6.2. Two related operator surfaces that don't fit the
//! other service modules:
//!
//! * **Device I/O** ([`onvif_soap::DEVICEIO`], `ver10/deviceIO`):
//!   relay outputs ([`get_relay_outputs`] / [`set_relay_output_state`]) —
//!   the dry-contact a camera can trip to drive a gate, lock or
//!   siren — digital inputs ([`get_digital_inputs`]) and audio
//!   sources ([`get_audio_sources`]).
//! * **On-screen display** ([`onvif_soap::MEDIA2`], `ver20/media`):
//!   the text overlays burned into the video
//!   ([`get_osds`] / [`create_text_osd`] / [`set_text_osd`] /
//!   [`delete_osd`]). OSD lives on the Media2 service on Profile-T
//!   cameras.
//!
//! Request builders and response parsers are pure so they unit
//! test against recorded fixtures. The public operations are
//! consumed by the operator admin-passthrough routes added in
//! Phase 7.6.3 (`crate::device_control`).

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use super::onvif_soap::{first_text, local_name, xml_escape, DEVICEIO, MEDIA2};

/// One relay output (dry contact).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RelayOutput {
    pub token: String,
    /// `Bistable` (latching) or `Monostable` (momentary).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// `open` or `closed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_state: Option<String>,
    /// `xs:duration` (e.g. `PT5S`), meaningful for monostable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay_time: Option<String>,
}

/// One digital input.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DigitalInput {
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_state: Option<String>,
}

/// One on-screen-display overlay.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Osd {
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_source_config_token: Option<String>,
    /// `Text` or `Image`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osd_type: Option<String>,
    /// `UpperLeft` / `UpperRight` / `LowerLeft` / `LowerRight` /
    /// `Custom`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    /// The plain-text string, for text OSDs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

// ---------------------------------------------------------------------------
// Device I/O
// ---------------------------------------------------------------------------

/// List the relay outputs.
pub async fn get_relay_outputs(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<Vec<RelayOutput>, String> {
    let resp = DEVICEIO
        .call(
            endpoint,
            username,
            password,
            "GetRelayOutputs",
            "<tmd:GetRelayOutputs/>",
        )
        .await?;
    Ok(parse_relay_outputs(&resp))
}

/// Set a relay output's logical state. `active` energises the
/// relay (or, for a monostable relay, pulses it).
pub async fn set_relay_output_state(
    endpoint: &str,
    username: &str,
    password: &str,
    relay_token: &str,
    active: bool,
) -> Result<(), String> {
    let body = format!(
        "<tmd:SetRelayOutputState><tmd:RelayOutputToken>{}</tmd:RelayOutputToken><tmd:LogicalState>{}</tmd:LogicalState></tmd:SetRelayOutputState>",
        xml_escape(relay_token),
        if active { "active" } else { "inactive" },
    );
    DEVICEIO
        .call(endpoint, username, password, "SetRelayOutputState", &body)
        .await
        .map(|_| ())
}

/// List the digital inputs.
pub async fn get_digital_inputs(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<Vec<DigitalInput>, String> {
    let resp = DEVICEIO
        .call(
            endpoint,
            username,
            password,
            "GetDigitalInputs",
            "<tmd:GetDigitalInputs/>",
        )
        .await?;
    Ok(parse_digital_inputs(&resp))
}

/// List the audio-source tokens.
pub async fn get_audio_sources(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<Vec<String>, String> {
    let resp = DEVICEIO
        .call(
            endpoint,
            username,
            password,
            "GetAudioSources",
            "<tmd:GetAudioSources/>",
        )
        .await?;
    Ok(collect_texts(&resp, "Token"))
}

// ---------------------------------------------------------------------------
// OSD (Media2)
// ---------------------------------------------------------------------------

/// List the OSDs, optionally scoped to one video-source
/// configuration.
pub async fn get_osds(
    endpoint: &str,
    username: &str,
    password: &str,
    config_token: Option<&str>,
) -> Result<Vec<Osd>, String> {
    let body = match config_token {
        Some(t) => format!(
            "<tr2:GetOSDs><tr2:ConfigurationToken>{}</tr2:ConfigurationToken></tr2:GetOSDs>",
            xml_escape(t)
        ),
        None => "<tr2:GetOSDs/>".to_string(),
    };
    let resp = MEDIA2
        .call(endpoint, username, password, "GetOSDs", &body)
        .await?;
    Ok(parse_osds(&resp))
}

/// Create a plain-text OSD on a video-source configuration.
/// Returns the OSD token the camera assigned.
pub async fn create_text_osd(
    endpoint: &str,
    username: &str,
    password: &str,
    video_source_config_token: &str,
    position: &str,
    text: &str,
) -> Result<String, String> {
    let osd = osd_text_body(None, video_source_config_token, position, text);
    let body = format!("<tr2:CreateOSD>{osd}</tr2:CreateOSD>");
    let resp = MEDIA2
        .call(endpoint, username, password, "CreateOSD", &body)
        .await?;
    first_text(&resp, "OSDToken").ok_or_else(|| "CreateOSD response missing OSDToken".to_string())
}

/// Update an existing plain-text OSD.
///
/// Uses read-modify-write: fetch the camera's current OSD and echo it back
/// verbatim with ONLY the plain text + position patched. Many firmwares
/// (Hikvision / Dahua) fault a stripped-down `SetOSD` with
/// `ter:InvalidParameter` when it drops fields they returned (FontColor,
/// FontSize, `IsPersistentText`, DateFormat, …) or changes the
/// `VideoSourceConfigurationToken`; echoing the camera's own OSD back
/// sidesteps that whole class of rejection. Falls back to a freshly
/// synthesized body when the current OSD can't be read (e.g. the camera is
/// unreachable, or the token isn't in `GetOSDs`).
pub async fn set_text_osd(
    endpoint: &str,
    username: &str,
    password: &str,
    osd_token: &str,
    video_source_config_token: &str,
    position: &str,
    text: &str,
) -> Result<(), String> {
    let osd = read_modify_osd(endpoint, username, password, osd_token, position, text)
        .await
        .unwrap_or_else(|| {
            osd_text_body(Some(osd_token), video_source_config_token, position, text)
        });
    let body = format!("<tr2:SetOSD>{osd}</tr2:SetOSD>");
    MEDIA2
        .call(endpoint, username, password, "SetOSD", &body)
        .await
        .map(|_| ())
}

/// Fetch the camera's OSDs and return the target OSD (by token) as a
/// ready-to-send `<tr2:OSD>` element with its plain text + position patched
/// and every other field preserved. `None` when the OSD can't be fetched or
/// the token isn't present, so the caller can synthesize a minimal body.
async fn read_modify_osd(
    endpoint: &str,
    username: &str,
    password: &str,
    osd_token: &str,
    position: &str,
    text: &str,
) -> Option<String> {
    let resp = MEDIA2
        .call(endpoint, username, password, "GetOSDs", "<tr2:GetOSDs/>")
        .await
        .ok()?;
    patch_osd_subtree(&resp, osd_token, position, text)
}

/// Delete an OSD.
pub async fn delete_osd(
    endpoint: &str,
    username: &str,
    password: &str,
    osd_token: &str,
) -> Result<(), String> {
    let body = format!(
        "<tr2:DeleteOSD><tr2:OSDToken>{}</tr2:OSDToken></tr2:DeleteOSD>",
        xml_escape(osd_token)
    );
    MEDIA2
        .call(endpoint, username, password, "DeleteOSD", &body)
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Request-body builders (pure)
// ---------------------------------------------------------------------------

fn osd_text_body(osd_token: Option<&str>, vsct: &str, position: &str, text: &str) -> String {
    let token_attr = osd_token
        .map(|t| format!(" token=\"{}\"", xml_escape(t)))
        .unwrap_or_default();
    format!(
        "<tr2:OSD{token_attr}><tt:VideoSourceConfigurationToken>{vsct}</tt:VideoSourceConfigurationToken><tt:Type>Text</tt:Type><tt:Position><tt:Type>{pos}</tt:Type></tt:Position><tt:TextString><tt:Type>Plain</tt:Type><tt:PlainText>{text}</tt:PlainText></tt:TextString></tr2:OSD>",
        vsct = xml_escape(vsct),
        pos = xml_escape(position),
        text = xml_escape(text),
    )
}

/// Re-emit the OSD whose `token` matches, taken from a `GetOSDs` response, as
/// a ready-to-send `<tr2:OSD token="…">…</tr2:OSD>` element: child element
/// prefixes are normalized to `tt:` (the SetOSD envelope declares `tt`/`tr2`)
/// and only the `Position/Type` and `TextString/PlainText` leaves are
/// patched. Every other field the camera returned (FontColor, FontSize,
/// `IsPersistentText`, DateFormat, …) is echoed back verbatim so the round
/// trip doesn't trip `ter:InvalidParameter`. `None` when the token isn't
/// found (the caller then synthesizes a minimal body).
///
/// A camera that has an OSD but has never had text set on it omits
/// `TextString` (or its `PlainText` child) from `GetOSDs` entirely. Patching
/// only the leaves that already exist would then echo the OSD back unchanged
/// — `SetOSD` returns success and the operator's text is silently dropped.
/// The missing subtrees are therefore synthesized: a `PlainText` is appended
/// inside an existing `TextString`, and a whole `TextString` (or `Position`)
/// is appended before the closing wrapper when absent.
fn patch_osd_subtree(
    response: &str,
    osd_token: &str,
    position: &str,
    text: &str,
) -> Option<String> {
    let mut reader = Reader::from_str(response);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut in_target = false;
    let mut found = false;
    // Local names of the OSD wrapper's currently-open child elements.
    let mut stack: Vec<String> = Vec::new();
    // Depth of a patched leaf (`Position/Type`, `TextString/PlainText`) whose
    // original text + close tag we suppress — we already emitted the full
    // replacement element at its start.
    let mut suppress: Option<usize> = None;
    // Which of the two patch targets the camera actually returned, so the
    // absent ones can be synthesized rather than silently skipped.
    let mut saw_text_string = false;
    let mut saw_plain_text = false;
    let mut saw_position = false;
    let mut saw_position_type = false;

    let plain_text_el = || format!("<tt:PlainText>{}</tt:PlainText>", xml_escape(text));
    let position_type_el = || format!("<tt:Type>{}</tt:Type>", xml_escape(position));

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(&e.name());
                if !in_target {
                    if is_osd_element(&n) && attr_str(&e, "token").as_deref() == Some(osd_token) {
                        in_target = true;
                        found = true;
                        out.push_str(&format!("<tr2:OSD token=\"{}\">", xml_escape(osd_token)));
                    }
                    // Elements outside the target OSD are skipped entirely.
                } else {
                    stack.push(n.clone());
                    if suppress.is_some() {
                        // Inside an already-emitted patched leaf — drop nested content.
                    } else {
                        let parent = (stack.len() >= 2).then(|| stack[stack.len() - 2].as_str());
                        if stack.len() == 1 && n == "TextString" {
                            saw_text_string = true;
                            out.push_str(&format!("<tt:{n}{}>", osd_attrs(&e)));
                        } else if stack.len() == 1 && n == "Position" {
                            saw_position = true;
                            out.push_str(&format!("<tt:{n}{}>", osd_attrs(&e)));
                        } else if n == "PlainText" && parent == Some("TextString") {
                            saw_plain_text = true;
                            out.push_str(&plain_text_el());
                            suppress = Some(stack.len());
                        } else if n == "Type" && parent == Some("Position") {
                            saw_position_type = true;
                            out.push_str(&position_type_el());
                            suppress = Some(stack.len());
                        } else {
                            out.push_str(&format!("<tt:{n}{}>", osd_attrs(&e)));
                        }
                    }
                }
            }
            Ok(Event::Empty(e)) if in_target && suppress.is_none() => {
                let n = local_name(&e.name());
                let parent = stack.last().map(String::as_str);
                if stack.is_empty() && n == "TextString" {
                    // `<tt:TextString/>` — expand it so the text lands.
                    saw_text_string = true;
                    saw_plain_text = true;
                    out.push_str(&format!(
                        "<tt:TextString><tt:Type>Plain</tt:Type>{}</tt:TextString>",
                        plain_text_el()
                    ));
                } else if stack.is_empty() && n == "Position" {
                    saw_position = true;
                    saw_position_type = true;
                    out.push_str(&format!(
                        "<tt:Position>{}</tt:Position>",
                        position_type_el()
                    ));
                } else if n == "PlainText" && parent == Some("TextString") {
                    saw_plain_text = true;
                    out.push_str(&plain_text_el());
                } else if n == "Type" && parent == Some("Position") {
                    saw_position_type = true;
                    out.push_str(&position_type_el());
                } else {
                    out.push_str(&format!("<tt:{n}{}/>", osd_attrs(&e)));
                }
            }
            Ok(Event::Text(t)) if in_target && suppress.is_none() => {
                if let Ok(s) = t.unescape() {
                    out.push_str(&xml_escape(&s));
                }
            }
            Ok(Event::End(_)) if in_target => {
                if stack.is_empty() {
                    // Closing the OSD wrapper itself. Anything the camera
                    // never reported is appended here, in XSD sequence order
                    // (Position precedes TextString).
                    if !saw_position {
                        out.push_str(&format!(
                            "<tt:Position>{}</tt:Position>",
                            position_type_el()
                        ));
                    }
                    if !saw_text_string {
                        out.push_str(&format!(
                            "<tt:TextString><tt:Type>Plain</tt:Type>{}</tt:TextString>",
                            plain_text_el()
                        ));
                    }
                    out.push_str("</tr2:OSD>");
                    break;
                }
                let d = stack.len();
                let n = stack.pop().unwrap_or_default();
                if suppress == Some(d) {
                    suppress = None; // full element already emitted; skip its close
                } else if suppress.is_none() {
                    // A `TextString` / `Position` the camera returned without
                    // the leaf we patch gets it appended before the close.
                    if d == 1 && n == "TextString" && !saw_plain_text {
                        saw_plain_text = true;
                        out.push_str(&plain_text_el());
                    } else if d == 1 && n == "Position" && !saw_position_type {
                        saw_position_type = true;
                        out.push_str(&position_type_el());
                    }
                    out.push_str(&format!("</tt:{n}>"));
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    (found && out.ends_with("</tr2:OSD>")).then_some(out)
}

/// Serialize a start element's attributes as ` local="value"` pairs, dropping
/// namespace declarations (the SetOSD envelope re-declares `tt`/`tr2`).
fn osd_attrs(e: &BytesStart) -> String {
    let mut s = String::new();
    for a in e.attributes().flatten() {
        let raw = std::str::from_utf8(a.key.as_ref()).unwrap_or("");
        if raw == "xmlns" || raw.starts_with("xmlns:") {
            continue;
        }
        let ln = raw.rsplit(':').next().unwrap_or(raw);
        let v = a
            .unescape_value()
            .map(|v| v.into_owned())
            .unwrap_or_default();
        s.push_str(&format!(" {ln}=\"{}\"", xml_escape(&v)));
    }
    s
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

/// Collect the trimmed text of every element with the given local
/// name (in document order).
fn collect_texts(body: &str, want: &str) -> Vec<String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut capture = false;
    let mut acc = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if local_name(&e.name()) == want => {
                capture = true;
                acc.clear();
            }
            Ok(Event::Text(t)) if capture => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) if local_name(&e.name()) == want => {
                let v = acc.trim().to_string();
                if !v.is_empty() {
                    out.push(v);
                }
                capture = false;
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn parse_relay_outputs(body: &str) -> Vec<RelayOutput> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut cur: Option<RelayOutput> = None;
    let mut capture: Option<&'static str> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(&e.name()).as_str() {
                "RelayOutputs" | "RelayOutput" => {
                    cur = Some(RelayOutput {
                        token: attr_str(&e, "token").unwrap_or_default(),
                        ..RelayOutput::default()
                    });
                }
                "Mode" if cur.is_some() => {
                    capture = Some("Mode");
                    acc.clear();
                }
                "IdleState" if cur.is_some() => {
                    capture = Some("IdleState");
                    acc.clear();
                }
                "DelayTime" if cur.is_some() => {
                    capture = Some("DelayTime");
                    acc.clear();
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => {
                let n = local_name(&e.name());
                if n == "RelayOutputs" || n == "RelayOutput" {
                    let token = attr_str(&e, "token").unwrap_or_default();
                    if !token.is_empty() {
                        out.push(RelayOutput {
                            token,
                            ..RelayOutput::default()
                        });
                    }
                }
            }
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let n = local_name(&e.name());
                if let Some(field) = capture {
                    if field == n {
                        if let Some(c) = cur.as_mut() {
                            let v = acc.trim().to_string();
                            match field {
                                "Mode" => c.mode = Some(v),
                                "IdleState" => c.idle_state = Some(v),
                                "DelayTime" => c.delay_time = Some(v),
                                _ => {}
                            }
                        }
                        capture = None;
                    }
                }
                if n == "RelayOutputs" || n == "RelayOutput" {
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

fn parse_digital_inputs(body: &str) -> Vec<DigitalInput> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut cur: Option<DigitalInput> = None;
    let mut capture = false;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(&e.name()).as_str() {
                "DigitalInputs" | "DigitalInput" => {
                    cur = Some(DigitalInput {
                        token: attr_str(&e, "token").unwrap_or_default(),
                        idle_state: None,
                    });
                }
                "IdleState" if cur.is_some() => {
                    capture = true;
                    acc.clear();
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => {
                let n = local_name(&e.name());
                if n == "DigitalInputs" || n == "DigitalInput" {
                    let token = attr_str(&e, "token").unwrap_or_default();
                    if !token.is_empty() {
                        out.push(DigitalInput {
                            token,
                            idle_state: None,
                        });
                    }
                }
            }
            Ok(Event::Text(t)) if capture => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let n = local_name(&e.name());
                if n == "IdleState" && capture {
                    if let Some(c) = cur.as_mut() {
                        c.idle_state = Some(acc.trim().to_string());
                    }
                    capture = false;
                }
                if n == "DigitalInputs" || n == "DigitalInput" {
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

const OSD_SECTIONS: &[&str] = &["Position", "TextString"];

fn current_section(stack: &[String]) -> Option<&'static str> {
    stack
        .iter()
        .rev()
        .find_map(|s| OSD_SECTIONS.iter().copied().find(|sec| *sec == s.as_str()))
}

/// True for the element that wraps one OSD configuration in a
/// `GetOSDsResponse`. ONVIF names the repeated element **`OSDs`**
/// (plural, per the ver10/ver20 media schema: `GetOSDsResponse/OSDs`
/// with `maxOccurs="unbounded"`), whereas the `SetOSD` / `CreateOSD`
/// request bodies use the singular `OSD`. Field cameras return the
/// plural form, so the parser accepts both spellings.
fn is_osd_element(name: &str) -> bool {
    name == "OSD" || name == "OSDs"
}

fn parse_osds(body: &str) -> Vec<Osd> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut out = Vec::new();
    let mut cur: Option<Osd> = None;
    let mut capture: Option<String> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(&e.name());
                if is_osd_element(&n) {
                    cur = Some(Osd {
                        token: attr_str(&e, "token").unwrap_or_default(),
                        ..Osd::default()
                    });
                }
                stack.push(n.clone());
                capture = Some(n);
                acc.clear();
            }
            Ok(Event::Empty(e)) => {
                let n = local_name(&e.name());
                if is_osd_element(&n) {
                    let token = attr_str(&e, "token").unwrap_or_default();
                    out.push(Osd {
                        token,
                        ..Osd::default()
                    });
                }
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
                            let section = current_section(&stack[..stack.len().saturating_sub(1)]);
                            match (section, n.as_str()) {
                                (None, "VideoSourceConfigurationToken") => {
                                    c.video_source_config_token = Some(v)
                                }
                                (None, "Type") => c.osd_type = Some(v),
                                (Some("Position"), "Type") => c.position = Some(v),
                                (Some("TextString"), "PlainText") => c.text = Some(v),
                                _ => {}
                            }
                        }
                    }
                }
                capture = None;
                stack.pop();
                if is_osd_element(&n) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(inner: &str) -> String {
        format!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tmd="http://www.onvif.org/ver10/deviceIO/wsdl" xmlns:tr2="http://www.onvif.org/ver20/media/wsdl"><s:Body>{inner}</s:Body></s:Envelope>"#
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
    fn parses_relay_outputs() {
        let body = wrap(
            r#"<tmd:GetRelayOutputsResponse>
                <tmd:RelayOutputs token="RelayOutputToken_0">
                    <tt:Properties><tt:Mode>Bistable</tt:Mode><tt:DelayTime>PT0S</tt:DelayTime><tt:IdleState>closed</tt:IdleState></tt:Properties>
                </tmd:RelayOutputs>
                <tmd:RelayOutputs token="RelayOutputToken_1">
                    <tt:Properties><tt:Mode>Monostable</tt:Mode><tt:DelayTime>PT5S</tt:DelayTime><tt:IdleState>open</tt:IdleState></tt:Properties>
                </tmd:RelayOutputs>
            </tmd:GetRelayOutputsResponse>"#,
        );
        let relays = parse_relay_outputs(&body);
        assert_eq!(relays.len(), 2);
        assert_eq!(relays[0].token, "RelayOutputToken_0");
        assert_eq!(relays[0].mode.as_deref(), Some("Bistable"));
        assert_eq!(relays[0].idle_state.as_deref(), Some("closed"));
        assert_eq!(relays[1].mode.as_deref(), Some("Monostable"));
        assert_eq!(relays[1].delay_time.as_deref(), Some("PT5S"));
    }

    #[test]
    fn parses_digital_inputs() {
        let body = wrap(
            r#"<tmd:GetDigitalInputsResponse>
                <tmd:DigitalInputs token="DigitalInput0"><tt:IdleState>closed</tt:IdleState></tmd:DigitalInputs>
                <tmd:DigitalInputs token="DigitalInput1"/>
            </tmd:GetDigitalInputsResponse>"#,
        );
        let inputs = parse_digital_inputs(&body);
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].token, "DigitalInput0");
        assert_eq!(inputs[0].idle_state.as_deref(), Some("closed"));
        assert_eq!(inputs[1].token, "DigitalInput1");
        assert_eq!(inputs[1].idle_state, None);
    }

    #[test]
    fn parses_audio_source_tokens() {
        let body = wrap(
            r#"<tmd:GetAudioSourcesResponse><tmd:Token>AudioSource0</tmd:Token><tmd:Token>AudioSource1</tmd:Token></tmd:GetAudioSourcesResponse>"#,
        );
        assert_eq!(
            collect_texts(&body, "Token"),
            vec!["AudioSource0", "AudioSource1"]
        );
    }

    #[test]
    fn set_relay_output_state_body_maps_active_flag() {
        let active = format!(
            "<tmd:SetRelayOutputState><tmd:RelayOutputToken>{}</tmd:RelayOutputToken><tmd:LogicalState>{}</tmd:LogicalState></tmd:SetRelayOutputState>",
            "R0", "active"
        );
        assert_well_formed(&wrap(&active));
        assert!(active.contains("<tmd:LogicalState>active</tmd:LogicalState>"));
    }

    #[test]
    fn parses_osds() {
        // Real cameras return each OSD as the **plural** `OSDs` element
        // (ONVIF `GetOSDsResponse/OSDs`, maxOccurs unbounded). Regression:
        // the parser used to match only the singular `OSD` (the spelling
        // used by SetOSD/CreateOSD *requests*) and so returned nothing for
        // a real GetOSDs response, silently defeating "name camera from OSD".
        let body = wrap(
            r#"<tr2:GetOSDsResponse>
                <tr2:OSDs token="OsdToken_100">
                    <tt:VideoSourceConfigurationToken>VideoSourceConfig_1</tt:VideoSourceConfigurationToken>
                    <tt:Type>Text</tt:Type>
                    <tt:Position><tt:Type>UpperLeft</tt:Type></tt:Position>
                    <tt:TextString><tt:Type>Plain</tt:Type><tt:PlainText>Front Door</tt:PlainText></tt:TextString>
                </tr2:OSDs>
                <tr2:OSDs token="OsdToken_101">
                    <tt:VideoSourceConfigurationToken>VideoSourceConfig_1</tt:VideoSourceConfigurationToken>
                    <tt:Type>Image</tt:Type>
                    <tt:Position><tt:Type>LowerRight</tt:Type></tt:Position>
                </tr2:OSDs>
            </tr2:GetOSDsResponse>"#,
        );
        let osds = parse_osds(&body);
        assert_eq!(osds.len(), 2);
        let o = &osds[0];
        assert_eq!(o.token, "OsdToken_100");
        assert_eq!(
            o.video_source_config_token.as_deref(),
            Some("VideoSourceConfig_1")
        );
        assert_eq!(o.osd_type.as_deref(), Some("Text"));
        assert_eq!(o.position.as_deref(), Some("UpperLeft"));
        assert_eq!(o.text.as_deref(), Some("Front Door"));
        // Image OSDs parse too (with no text); osd_display_name skips them.
        assert_eq!(osds[1].osd_type.as_deref(), Some("Image"));
        assert_eq!(osds[1].text, None);
    }

    #[test]
    fn patch_osd_subtree_preserves_camera_fields() {
        // A camera OSD carrying extra fields (IsPersistentText, FontSize,
        // FontColor) that strict firmwares require echoed back on SetOSD.
        let resp = wrap(
            r#"<tr2:GetOSDsResponse>
                <tr2:OSDs token="OsdToken_100">
                    <tt:VideoSourceConfigurationToken>VideoSourceConfig_1</tt:VideoSourceConfigurationToken>
                    <tt:Type>Text</tt:Type>
                    <tt:Position><tt:Type>UpperLeft</tt:Type></tt:Position>
                    <tt:TextString IsPersistentText="true"><tt:Type>Plain</tt:Type><tt:FontSize>32</tt:FontSize><tt:FontColor><tt:Color X="1" Y="0" Z="0"/></tt:FontColor><tt:PlainText>Front Door</tt:PlainText></tt:TextString>
                </tr2:OSDs>
                <tr2:OSDs token="OsdToken_101"><tt:Type>Image</tt:Type></tr2:OSDs>
            </tr2:GetOSDsResponse>"#,
        );
        let osd =
            patch_osd_subtree(&resp, "OsdToken_100", "LowerRight", "Dock 7").expect("patched");
        // Well-formed as a SetOSD body under the standard tt/tr2 envelope.
        assert_well_formed(&wrap(&format!("<tr2:SetOSD>{osd}</tr2:SetOSD>")));
        // Wrapper renamed to the singular request element, carrying the token.
        assert!(osd.starts_with("<tr2:OSD token=\"OsdToken_100\">"), "{osd}");
        assert!(osd.ends_with("</tr2:OSD>"), "{osd}");
        // Only the text + position leaves change.
        assert!(osd.contains("<tt:PlainText>Dock 7</tt:PlainText>"), "{osd}");
        assert!(
            osd.contains("<tt:Position><tt:Type>LowerRight</tt:Type></tt:Position>"),
            "{osd}"
        );
        assert!(!osd.contains("Front Door"), "{osd}");
        assert!(!osd.contains("UpperLeft"), "{osd}");
        // Everything else the camera returned is echoed back verbatim.
        assert!(
            osd.contains("<tt:VideoSourceConfigurationToken>VideoSourceConfig_1</tt:VideoSourceConfigurationToken>"),
            "{osd}"
        );
        assert!(osd.contains("IsPersistentText=\"true\""), "{osd}");
        assert!(osd.contains("<tt:FontSize>32</tt:FontSize>"), "{osd}");
        assert!(osd.contains("<tt:Color X=\"1\" Y=\"0\" Z=\"0\"/>"), "{osd}");
        // The top-level Text type and the TextString Plain type are NOT the
        // position leaf and must survive unchanged.
        assert!(osd.contains("<tt:Type>Text</tt:Type>"), "{osd}");
        assert!(osd.contains("<tt:Type>Plain</tt:Type>"), "{osd}");
    }

    #[test]
    fn patch_osd_subtree_patches_empty_plaintext() {
        // A self-closed <PlainText/> must still receive the new text.
        let resp = wrap(
            r#"<tr2:GetOSDsResponse><tr2:OSDs token="T">
                <tt:VideoSourceConfigurationToken>V1</tt:VideoSourceConfigurationToken>
                <tt:Type>Text</tt:Type>
                <tt:Position><tt:Type>UpperLeft</tt:Type></tt:Position>
                <tt:TextString><tt:Type>Plain</tt:Type><tt:PlainText/></tt:TextString>
            </tr2:OSDs></tr2:GetOSDsResponse>"#,
        );
        let osd = patch_osd_subtree(&resp, "T", "UpperRight", "Hello").expect("patched");
        assert!(osd.contains("<tt:PlainText>Hello</tt:PlainText>"), "{osd}");
        assert!(
            osd.contains("<tt:Position><tt:Type>UpperRight</tt:Type></tt:Position>"),
            "{osd}"
        );
    }

    #[test]
    fn patch_osd_subtree_synthesizes_missing_text_string() {
        // A camera that has never had text set on this OSD omits TextString
        // from GetOSDs entirely (this is verbatim what the reference PTZ
        // returns). Echoing it back unchanged made SetOSD a silent no-op:
        // the request succeeded and the operator's text never landed.
        let resp = wrap(
            r#"<tr2:GetOSDsResponse><tr2:OSDs token="OsdToken_101">
                <tt:VideoSourceConfigurationToken>VideoSourceToken</tt:VideoSourceConfigurationToken>
                <tt:Type>Text</tt:Type>
                <tt:Position><tt:Type>Custom</tt:Type></tt:Position>
            </tr2:OSDs></tr2:GetOSDsResponse>"#,
        );
        let osd = patch_osd_subtree(&resp, "OsdToken_101", "UpperLeft", "Gate 4").expect("patched");
        assert_well_formed(&wrap(&format!("<tr2:SetOSD>{osd}</tr2:SetOSD>")));
        assert!(
            osd.contains("<tt:TextString><tt:Type>Plain</tt:Type><tt:PlainText>Gate 4</tt:PlainText></tt:TextString>"),
            "{osd}"
        );
        assert!(
            osd.contains("<tt:Position><tt:Type>UpperLeft</tt:Type></tt:Position>"),
            "{osd}"
        );
        // Synthesized TextString goes after Position, per the XSD sequence.
        assert!(
            osd.find("<tt:Position>") < osd.find("<tt:TextString>"),
            "{osd}"
        );
        assert!(!osd.contains("Custom"), "{osd}");
    }

    #[test]
    fn patch_osd_subtree_appends_plaintext_into_existing_text_string() {
        // TextString present but with no PlainText child — the text must be
        // appended inside it rather than dropped.
        let resp = wrap(
            r#"<tr2:GetOSDsResponse><tr2:OSDs token="T">
                <tt:Type>Text</tt:Type>
                <tt:Position><tt:Type>UpperLeft</tt:Type></tt:Position>
                <tt:TextString><tt:Type>Plain</tt:Type><tt:FontSize>32</tt:FontSize></tt:TextString>
            </tr2:OSDs></tr2:GetOSDsResponse>"#,
        );
        let osd = patch_osd_subtree(&resp, "T", "LowerLeft", "Dock 7").expect("patched");
        assert_well_formed(&wrap(&format!("<tr2:SetOSD>{osd}</tr2:SetOSD>")));
        assert!(
            osd.contains(
                "<tt:FontSize>32</tt:FontSize><tt:PlainText>Dock 7</tt:PlainText></tt:TextString>"
            ),
            "{osd}"
        );
    }

    #[test]
    fn patch_osd_subtree_synthesizes_missing_position() {
        let resp = wrap(
            r#"<tr2:GetOSDsResponse><tr2:OSDs token="T">
                <tt:Type>Text</tt:Type>
                <tt:TextString><tt:Type>Plain</tt:Type><tt:PlainText>old</tt:PlainText></tt:TextString>
            </tr2:OSDs></tr2:GetOSDsResponse>"#,
        );
        let osd = patch_osd_subtree(&resp, "T", "LowerRight", "new").expect("patched");
        assert_well_formed(&wrap(&format!("<tr2:SetOSD>{osd}</tr2:SetOSD>")));
        assert!(
            osd.contains("<tt:Position><tt:Type>LowerRight</tt:Type></tt:Position>"),
            "{osd}"
        );
        assert!(osd.contains("<tt:PlainText>new</tt:PlainText>"), "{osd}");
        assert!(!osd.contains("old"), "{osd}");
    }

    #[test]
    fn patch_osd_subtree_expands_self_closed_text_string() {
        let resp = wrap(
            r#"<tr2:GetOSDsResponse><tr2:OSDs token="T">
                <tt:Type>Text</tt:Type>
                <tt:Position><tt:Type>UpperLeft</tt:Type></tt:Position>
                <tt:TextString/>
            </tr2:OSDs></tr2:GetOSDsResponse>"#,
        );
        let osd = patch_osd_subtree(&resp, "T", "UpperLeft", "Hi").expect("patched");
        assert_well_formed(&wrap(&format!("<tr2:SetOSD>{osd}</tr2:SetOSD>")));
        assert!(osd.contains("<tt:PlainText>Hi</tt:PlainText>"), "{osd}");
    }

    #[test]
    fn patch_osd_subtree_missing_token_is_none() {
        let resp = wrap(
            r#"<tr2:GetOSDsResponse><tr2:OSDs token="A"><tt:Type>Text</tt:Type></tr2:OSDs></tr2:GetOSDsResponse>"#,
        );
        assert!(patch_osd_subtree(&resp, "Z", "UpperLeft", "x").is_none());
    }

    #[test]
    fn parses_osds_accepts_singular_element() {
        // `SetOSD`/`CreateOSD` bodies (and a few non-conforming cameras)
        // use the singular `OSD`; the parser accepts both spellings.
        let body = wrap(
            r#"<tr2:GetOSDsResponse>
                <tr2:OSD token="OsdToken_7">
                    <tt:Type>Text</tt:Type>
                    <tt:TextString><tt:Type>Plain</tt:Type><tt:PlainText>Bay 3</tt:PlainText></tt:TextString>
                </tr2:OSD>
            </tr2:GetOSDsResponse>"#,
        );
        let osds = parse_osds(&body);
        assert_eq!(osds.len(), 1);
        assert_eq!(osds[0].token, "OsdToken_7");
        assert_eq!(osds[0].text.as_deref(), Some("Bay 3"));
    }

    #[test]
    fn osd_text_body_is_well_formed_and_escapes_text() {
        let create = osd_text_body(None, "VSC_1", "LowerRight", "Bay <3> & co");
        assert_well_formed(&wrap(&format!("<tr2:CreateOSD>{create}</tr2:CreateOSD>")));
        assert!(create.contains("<tt:Type>LowerRight</tt:Type>"), "{create}");
        assert!(create.contains("Bay &lt;3&gt; &amp; co"), "{create}");
        assert!(!create.contains(" token="), "{create}");

        let set = osd_text_body(Some("OsdToken_5"), "VSC_1", "UpperLeft", "Hi");
        assert!(set.contains("token=\"OsdToken_5\""), "{set}");
    }
}
