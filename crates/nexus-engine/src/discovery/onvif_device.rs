//! ONVIF core device service client (`ver10/device`).
//!
//! Phase 7.6.2. The "what is this box and how do I configure its
//! plumbing" service: device identity ([`get_device_information`]),
//! service discovery ([`get_capabilities`] / [`get_services`]),
//! the system clock ([`get_system_date_and_time`] /
//! `set_system_time_*`), the NTP client config ([`get_ntp`] /
//! [`set_ntp`]), the diagnostic system log ([`get_system_log`]),
//! and the big red button ([`system_reboot`]).
//!
//! Every request funnels through [`onvif_soap::DEVICE`]; the
//! request builders and response parsers are pure functions so
//! they unit-test against recorded fixtures without a network.
//!
//! The public operations are the device-control surface consumed
//! by the operator admin-passthrough routes added in Phase 7.6.3
//! (`crate::device_control`).

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use super::onvif_soap::{collect_first_texts, first_text, local_name, xml_escape, DEVICE};

/// Static device identity (`GetDeviceInformation`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DeviceInformation {
    pub manufacturer: String,
    pub model: String,
    pub firmware_version: String,
    pub serial_number: String,
    pub hardware_id: String,
}

/// One advertised ONVIF service (`GetServices`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OnvifServiceEntry {
    pub namespace: String,
    pub xaddr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Per-category service endpoint URLs extracted from
/// `GetCapabilities`. Each is the `XAddr` the camera serves that
/// category at (usually all identical, occasionally split).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Capabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ptz_xaddr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imaging_xaddr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_xaddr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_io_xaddr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events_xaddr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analytics_xaddr: Option<String>,
}

/// System clock state (`GetSystemDateAndTime`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SystemDateTime {
    /// `Manual` or `NTP`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_time_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daylight_savings: Option<bool>,
    /// POSIX TZ string (e.g. `CST6CDT`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Reassembled `YYYY-MM-DDTHH:MM:SSZ` from the UTC fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utc_datetime: Option<String>,
}

/// NTP client configuration (`GetNTP`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NtpInformation {
    pub from_dhcp: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<String>,
}

// ---------------------------------------------------------------------------
// Identity + discovery
// ---------------------------------------------------------------------------

/// Read the device's static identity.
pub async fn get_device_information(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<DeviceInformation, String> {
    let resp = DEVICE
        .call(
            endpoint,
            username,
            password,
            "GetDeviceInformation",
            "<tds:GetDeviceInformation/>",
        )
        .await?;
    Ok(parse_device_information(&resp))
}

/// Read the per-category service endpoint URLs.
pub async fn get_capabilities(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<Capabilities, String> {
    let body = "<tds:GetCapabilities><tds:Category>All</tds:Category></tds:GetCapabilities>";
    let resp = DEVICE
        .call(endpoint, username, password, "GetCapabilities", body)
        .await?;
    Ok(parse_capabilities(&resp))
}

/// Read the advertised ONVIF services.
pub async fn get_services(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<Vec<OnvifServiceEntry>, String> {
    let body =
        "<tds:GetServices><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>";
    let resp = DEVICE
        .call(endpoint, username, password, "GetServices", body)
        .await?;
    Ok(parse_services(&resp))
}

// ---------------------------------------------------------------------------
// Clock + NTP
// ---------------------------------------------------------------------------

/// Read the system clock state.
pub async fn get_system_date_and_time(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<SystemDateTime, String> {
    let resp = DEVICE
        .call(
            endpoint,
            username,
            password,
            "GetSystemDateAndTime",
            "<tds:GetSystemDateAndTime/>",
        )
        .await?;
    Ok(parse_system_date_time(&resp))
}

/// Switch the clock to manual mode and set it to the given UTC
/// wall-clock components.
#[allow(clippy::too_many_arguments)]
pub async fn set_system_time_manual(
    endpoint: &str,
    username: &str,
    password: &str,
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    timezone: &str,
    daylight_savings: bool,
) -> Result<(), String> {
    let body = set_time_manual_body(
        year,
        month,
        day,
        hour,
        minute,
        second,
        timezone,
        daylight_savings,
    );
    DEVICE
        .call(endpoint, username, password, "SetSystemDateAndTime", &body)
        .await
        .map(|_| ())
}

/// Switch the clock to NTP mode (the camera then syncs from its
/// configured NTP servers — see [`set_ntp`]).
pub async fn set_system_time_ntp(
    endpoint: &str,
    username: &str,
    password: &str,
    timezone: &str,
    daylight_savings: bool,
) -> Result<(), String> {
    let body = format!(
        "<tds:SetSystemDateAndTime><tds:DateTimeType>NTP</tds:DateTimeType><tds:DaylightSavings>{}</tds:DaylightSavings><tds:TimeZone><tt:TZ>{}</tt:TZ></tds:TimeZone></tds:SetSystemDateAndTime>",
        daylight_savings,
        xml_escape(timezone),
    );
    DEVICE
        .call(endpoint, username, password, "SetSystemDateAndTime", &body)
        .await
        .map(|_| ())
}

/// Read the NTP client configuration.
pub async fn get_ntp(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<NtpInformation, String> {
    let resp = DEVICE
        .call(endpoint, username, password, "GetNTP", "<tds:GetNTP/>")
        .await?;
    Ok(parse_ntp(&resp))
}

/// Set the NTP server(s). When `from_dhcp` is true the manual
/// server is ignored and the camera takes NTP from DHCP.
pub async fn set_ntp(
    endpoint: &str,
    username: &str,
    password: &str,
    from_dhcp: bool,
    server: Option<&str>,
) -> Result<(), String> {
    let body = set_ntp_body(from_dhcp, server);
    DEVICE
        .call(endpoint, username, password, "SetNTP", &body)
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Diagnostics + lifecycle
// ---------------------------------------------------------------------------

/// Fetch the device system log. `log_type` is `System` or
/// `Access`.
pub async fn get_system_log(
    endpoint: &str,
    username: &str,
    password: &str,
    log_type: &str,
) -> Result<String, String> {
    let body = format!(
        "<tds:GetSystemLog><tds:LogType>{}</tds:LogType></tds:GetSystemLog>",
        xml_escape(log_type)
    );
    let resp = DEVICE
        .call(endpoint, username, password, "GetSystemLog", &body)
        .await?;
    Ok(first_text(&resp, "String").unwrap_or_default())
}

/// Reboot the device. Returns the camera's acknowledgement
/// message (e.g. `Rebooting in 5 seconds`).
pub async fn system_reboot(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<String, String> {
    let resp = DEVICE
        .call(
            endpoint,
            username,
            password,
            "SystemReboot",
            "<tds:SystemReboot/>",
        )
        .await?;
    Ok(first_text(&resp, "Message").unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Request-body builders (pure)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn set_time_manual_body(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    timezone: &str,
    daylight_savings: bool,
) -> String {
    format!(
        "<tds:SetSystemDateAndTime><tds:DateTimeType>Manual</tds:DateTimeType><tds:DaylightSavings>{dst}</tds:DaylightSavings><tds:TimeZone><tt:TZ>{tz}</tt:TZ></tds:TimeZone><tds:UTCDateTime><tt:Time><tt:Hour>{hour}</tt:Hour><tt:Minute>{minute}</tt:Minute><tt:Second>{second}</tt:Second></tt:Time><tt:Date><tt:Year>{year}</tt:Year><tt:Month>{month}</tt:Month><tt:Day>{day}</tt:Day></tt:Date></tds:UTCDateTime></tds:SetSystemDateAndTime>",
        dst = daylight_savings,
        tz = xml_escape(timezone),
    )
}

fn set_ntp_body(from_dhcp: bool, server: Option<&str>) -> String {
    let manual = if from_dhcp {
        String::new()
    } else if let Some(s) = server {
        // IPv4 literal vs DNS name decides the ONVIF Type tag.
        let is_ipv4 = s.parse::<std::net::Ipv4Addr>().is_ok();
        if is_ipv4 {
            format!(
                "<tds:NTPManual><tt:Type>IPv4</tt:Type><tt:IPv4Address>{}</tt:IPv4Address></tds:NTPManual>",
                xml_escape(s)
            )
        } else {
            format!(
                "<tds:NTPManual><tt:Type>DNS</tt:Type><tt:DNSname>{}</tt:DNSname></tds:NTPManual>",
                xml_escape(s)
            )
        }
    } else {
        String::new()
    };
    format!("<tds:SetNTP><tds:FromDHCP>{from_dhcp}</tds:FromDHCP>{manual}</tds:SetNTP>")
}

// ---------------------------------------------------------------------------
// Response parsers (pure)
// ---------------------------------------------------------------------------

fn parse_device_information(body: &str) -> DeviceInformation {
    let mut m = collect_first_texts(
        body,
        &[
            "Manufacturer",
            "Model",
            "FirmwareVersion",
            "SerialNumber",
            "HardwareId",
        ],
    );
    DeviceInformation {
        manufacturer: m.remove("Manufacturer").unwrap_or_default(),
        model: m.remove("Model").unwrap_or_default(),
        firmware_version: m.remove("FirmwareVersion").unwrap_or_default(),
        serial_number: m.remove("SerialNumber").unwrap_or_default(),
        hardware_id: m.remove("HardwareId").unwrap_or_default(),
    }
}

fn parse_services(body: &str) -> Vec<OnvifServiceEntry> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut in_service = false;
    let mut ns = String::new();
    let mut xaddr = String::new();
    let mut major = String::new();
    let mut minor = String::new();
    let mut capture: Option<&'static str> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(&e.name()).as_str() {
                "Service" => {
                    in_service = true;
                    ns.clear();
                    xaddr.clear();
                    major.clear();
                    minor.clear();
                }
                "Namespace" if in_service => {
                    capture = Some("Namespace");
                    acc.clear();
                }
                "XAddr" if in_service => {
                    capture = Some("XAddr");
                    acc.clear();
                }
                "Major" if in_service => {
                    capture = Some("Major");
                    acc.clear();
                }
                "Minor" if in_service => {
                    capture = Some("Minor");
                    acc.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let n = local_name(&e.name());
                if let Some(field) = capture {
                    if field == n {
                        let val = acc.trim().to_string();
                        match field {
                            "Namespace" => ns = val,
                            "XAddr" => xaddr = val,
                            "Major" => major = val,
                            "Minor" => minor = val,
                            _ => {}
                        }
                        capture = None;
                    }
                }
                if n == "Service" {
                    in_service = false;
                    if !ns.is_empty() || !xaddr.is_empty() {
                        let version = if major.is_empty() {
                            None
                        } else if minor.is_empty() {
                            Some(major.clone())
                        } else {
                            Some(format!("{major}.{minor}"))
                        };
                        out.push(OnvifServiceEntry {
                            namespace: ns.clone(),
                            xaddr: xaddr.clone(),
                            version,
                        });
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn parse_capabilities(body: &str) -> Capabilities {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut caps = Capabilities::default();
    // Which capability category we're inside.
    let mut category: Option<&'static str> = None;
    let mut capture_xaddr = false;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(&e.name()).as_str() {
                "PTZ" => category = Some("ptz"),
                "Imaging" => category = Some("imaging"),
                "Media" => category = Some("media"),
                "DeviceIO" => category = Some("device_io"),
                "Events" => category = Some("events"),
                "Analytics" => category = Some("analytics"),
                "XAddr" if category.is_some() => {
                    capture_xaddr = true;
                    acc.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if capture_xaddr => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => match local_name(&e.name()).as_str() {
                "XAddr" if capture_xaddr => {
                    let val = acc.trim().to_string();
                    if !val.is_empty() {
                        match category {
                            Some("ptz") => {
                                caps.ptz_xaddr.get_or_insert(val);
                            }
                            Some("imaging") => {
                                caps.imaging_xaddr.get_or_insert(val);
                            }
                            Some("media") => {
                                caps.media_xaddr.get_or_insert(val);
                            }
                            Some("device_io") => {
                                caps.device_io_xaddr.get_or_insert(val);
                            }
                            Some("events") => {
                                caps.events_xaddr.get_or_insert(val);
                            }
                            Some("analytics") => {
                                caps.analytics_xaddr.get_or_insert(val);
                            }
                            _ => {}
                        }
                    }
                    capture_xaddr = false;
                }
                "PTZ" | "Imaging" | "Media" | "DeviceIO" | "Events" | "Analytics" => {
                    category = None;
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    caps
}

fn parse_system_date_time(body: &str) -> SystemDateTime {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = SystemDateTime::default();
    let mut in_utc = false;
    let (mut y, mut mo, mut d, mut h, mut mi, mut s) = (None, None, None, None, None, None);
    let mut capture: Option<&'static str> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(&e.name()).as_str() {
                "UTCDateTime" => in_utc = true,
                "DateTimeType" => {
                    capture = Some("DateTimeType");
                    acc.clear();
                }
                "DaylightSavings" => {
                    capture = Some("DaylightSavings");
                    acc.clear();
                }
                "TZ" => {
                    capture = Some("TZ");
                    acc.clear();
                }
                "Year" if in_utc => {
                    capture = Some("Year");
                    acc.clear();
                }
                "Month" if in_utc => {
                    capture = Some("Month");
                    acc.clear();
                }
                "Day" if in_utc => {
                    capture = Some("Day");
                    acc.clear();
                }
                "Hour" if in_utc => {
                    capture = Some("Hour");
                    acc.clear();
                }
                "Minute" if in_utc => {
                    capture = Some("Minute");
                    acc.clear();
                }
                "Second" if in_utc => {
                    capture = Some("Second");
                    acc.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(txt) = t.unescape() {
                    acc.push_str(&txt);
                }
            }
            Ok(Event::End(e)) => {
                let n = local_name(&e.name());
                if let Some(field) = capture {
                    if field == n {
                        let v = acc.trim().to_string();
                        match field {
                            "DateTimeType" => out.date_time_type = Some(v),
                            "DaylightSavings" => {
                                out.daylight_savings = Some(v.eq_ignore_ascii_case("true"))
                            }
                            "TZ" => out.timezone = Some(v),
                            "Year" => y = v.parse::<u16>().ok(),
                            "Month" => mo = v.parse::<u8>().ok(),
                            "Day" => d = v.parse::<u8>().ok(),
                            "Hour" => h = v.parse::<u8>().ok(),
                            "Minute" => mi = v.parse::<u8>().ok(),
                            "Second" => s = v.parse::<u8>().ok(),
                            _ => {}
                        }
                        capture = None;
                    }
                }
                if n == "UTCDateTime" {
                    in_utc = false;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    if let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) = (y, mo, d, h, mi, s) {
        out.utc_datetime = Some(format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"));
    }
    out
}

fn parse_ntp(body: &str) -> NtpInformation {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = NtpInformation::default();
    let mut capture: Option<&'static str> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match local_name(&e.name()).as_str() {
                "FromDHCP" => {
                    capture = Some("FromDHCP");
                    acc.clear();
                }
                "IPv4Address" => {
                    capture = Some("IPv4Address");
                    acc.clear();
                }
                "IPv6Address" => {
                    capture = Some("IPv6Address");
                    acc.clear();
                }
                "DNSname" => {
                    capture = Some("DNSname");
                    acc.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let n = local_name(&e.name());
                if let Some(field) = capture {
                    if field == n {
                        let v = acc.trim().to_string();
                        match field {
                            "FromDHCP" => out.from_dhcp = v.eq_ignore_ascii_case("true"),
                            "IPv4Address" | "IPv6Address" | "DNSname" if !v.is_empty() => {
                                out.servers.push(v)
                            }
                            _ => {}
                        }
                        capture = None;
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
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><s:Body>{inner}</s:Body></s:Envelope>"#
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
    fn parses_device_information() {
        let body = wrap(
            r#"<tds:GetDeviceInformationResponse>
                <tds:Manufacturer>Hikvision</tds:Manufacturer>
                <tds:Model>DS-2CD2085FWD-I</tds:Model>
                <tds:FirmwareVersion>V5.6.3</tds:FirmwareVersion>
                <tds:SerialNumber>DS-2CD208520200101</tds:SerialNumber>
                <tds:HardwareId>88</tds:HardwareId>
            </tds:GetDeviceInformationResponse>"#,
        );
        let info = parse_device_information(&body);
        assert_eq!(info.manufacturer, "Hikvision");
        assert_eq!(info.model, "DS-2CD2085FWD-I");
        assert_eq!(info.firmware_version, "V5.6.3");
        assert_eq!(info.serial_number, "DS-2CD208520200101");
        assert_eq!(info.hardware_id, "88");
    }

    #[test]
    fn parses_services() {
        let body = wrap(
            r#"<tds:GetServicesResponse>
                <tds:Service>
                    <tds:Namespace>http://www.onvif.org/ver10/device/wsdl</tds:Namespace>
                    <tds:XAddr>http://192.168.1.64/onvif/device_service</tds:XAddr>
                    <tds:Version><tt:Major>2</tt:Major><tt:Minor>60</tt:Minor></tds:Version>
                </tds:Service>
                <tds:Service>
                    <tds:Namespace>http://www.onvif.org/ver20/ptz/wsdl</tds:Namespace>
                    <tds:XAddr>http://192.168.1.64/onvif/ptz_service</tds:XAddr>
                    <tds:Version><tt:Major>2</tt:Major><tt:Minor>60</tt:Minor></tds:Version>
                </tds:Service>
            </tds:GetServicesResponse>"#,
        );
        let svcs = parse_services(&body);
        assert_eq!(svcs.len(), 2);
        assert_eq!(svcs[0].namespace, "http://www.onvif.org/ver10/device/wsdl");
        assert_eq!(svcs[0].xaddr, "http://192.168.1.64/onvif/device_service");
        assert_eq!(svcs[0].version.as_deref(), Some("2.60"));
        assert_eq!(svcs[1].namespace, "http://www.onvif.org/ver20/ptz/wsdl");
    }

    #[test]
    fn parses_capabilities_xaddrs() {
        let body = wrap(
            r#"<tds:GetCapabilitiesResponse><tds:Capabilities>
                <tt:Media><tt:XAddr>http://192.168.1.64/onvif/media</tt:XAddr></tt:Media>
                <tt:PTZ><tt:XAddr>http://192.168.1.64/onvif/ptz</tt:XAddr></tt:PTZ>
                <tt:Imaging><tt:XAddr>http://192.168.1.64/onvif/imaging</tt:XAddr></tt:Imaging>
                <tt:Events><tt:XAddr>http://192.168.1.64/onvif/events</tt:XAddr></tt:Events>
            </tds:Capabilities></tds:GetCapabilitiesResponse>"#,
        );
        let caps = parse_capabilities(&body);
        assert_eq!(
            caps.media_xaddr.as_deref(),
            Some("http://192.168.1.64/onvif/media")
        );
        assert_eq!(
            caps.ptz_xaddr.as_deref(),
            Some("http://192.168.1.64/onvif/ptz")
        );
        assert_eq!(
            caps.imaging_xaddr.as_deref(),
            Some("http://192.168.1.64/onvif/imaging")
        );
        assert_eq!(
            caps.events_xaddr.as_deref(),
            Some("http://192.168.1.64/onvif/events")
        );
        assert_eq!(caps.analytics_xaddr, None);
    }

    #[test]
    fn parses_system_date_time() {
        let body = wrap(
            r#"<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime>
                <tt:DateTimeType>NTP</tt:DateTimeType>
                <tt:DaylightSavings>false</tt:DaylightSavings>
                <tt:TimeZone><tt:TZ>CST6CDT</tt:TZ></tt:TimeZone>
                <tt:UTCDateTime>
                    <tt:Time><tt:Hour>14</tt:Hour><tt:Minute>5</tt:Minute><tt:Second>9</tt:Second></tt:Time>
                    <tt:Date><tt:Year>2024</tt:Year><tt:Month>5</tt:Month><tt:Day>1</tt:Day></tt:Date>
                </tt:UTCDateTime>
            </tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>"#,
        );
        let dt = parse_system_date_time(&body);
        assert_eq!(dt.date_time_type.as_deref(), Some("NTP"));
        assert_eq!(dt.daylight_savings, Some(false));
        assert_eq!(dt.timezone.as_deref(), Some("CST6CDT"));
        assert_eq!(dt.utc_datetime.as_deref(), Some("2024-05-01T14:05:09Z"));
    }

    #[test]
    fn parses_ntp_information() {
        let body = wrap(
            r#"<tds:GetNTPResponse><tds:NTPInformation>
                <tt:FromDHCP>false</tt:FromDHCP>
                <tt:NTPManual><tt:Type>DNS</tt:Type><tt:DNSname>pool.ntp.org</tt:DNSname></tt:NTPManual>
            </tds:NTPInformation></tds:GetNTPResponse>"#,
        );
        let ntp = parse_ntp(&body);
        assert!(!ntp.from_dhcp);
        assert_eq!(ntp.servers, vec!["pool.ntp.org"]);
    }

    #[test]
    fn set_time_manual_body_is_well_formed_and_zero_pads() {
        let body = set_time_manual_body(2024, 5, 1, 9, 3, 7, "UTC0", false);
        assert_well_formed(&wrap(&body));
        assert!(body.contains("<tds:DateTimeType>Manual</tds:DateTimeType>"));
        assert!(body.contains("<tt:Year>2024</tt:Year>"), "{body}");
        assert!(body.contains("<tt:Month>5</tt:Month>"), "{body}");
        assert!(body.contains("<tt:Hour>9</tt:Hour>"), "{body}");
    }

    #[test]
    fn set_ntp_body_picks_ipv4_vs_dns_type() {
        let ip = set_ntp_body(false, Some("192.168.1.1"));
        assert_well_formed(&wrap(&ip));
        assert!(ip.contains("<tt:Type>IPv4</tt:Type>"), "{ip}");
        assert!(
            ip.contains("<tt:IPv4Address>192.168.1.1</tt:IPv4Address>"),
            "{ip}"
        );

        let dns = set_ntp_body(false, Some("time.cloudflare.com"));
        assert!(dns.contains("<tt:Type>DNS</tt:Type>"), "{dns}");
        assert!(
            dns.contains("<tt:DNSname>time.cloudflare.com</tt:DNSname>"),
            "{dns}"
        );

        let dhcp = set_ntp_body(true, Some("192.168.1.1"));
        assert!(dhcp.contains("<tds:FromDHCP>true</tds:FromDHCP>"), "{dhcp}");
        assert!(!dhcp.contains("NTPManual"), "{dhcp}");
    }
}
