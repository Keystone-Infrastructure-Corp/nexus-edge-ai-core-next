//! IP speakers (ADR-091): devices a core owns, assigned to cameras as their
//! talk-down path. Edited through `/v1/admin/speakers` and stored in the
//! `speakers` table; credentials stay on the edge.

use serde::{Deserialize, Serialize};

pub type SpeakerId = i64;

/// How the edge reaches a speaker. `sip` is the only driver built; others
/// are additive (ADR-091).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerDriver {
    #[default]
    Sip,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpeakerConfig {
    #[serde(default)]
    pub id: SpeakerId,
    pub name: String,
    /// IP address or hostname, no scheme, port or path.
    pub host: String,
    #[serde(default = "default_sip_port")]
    pub port: u16,
    #[serde(default)]
    pub driver: SpeakerDriver,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// The speaker's ONVIF Media service URL (http or https, on `host`), used
    /// to set its output level on the device. Absent: software volume only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onvif_endpoint: Option<String>,
}

fn default_sip_port() -> u16 {
    5060
}

// Hand-written so a `{:?}` in a log line can never print the password.
impl std::fmt::Debug for SpeakerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeakerConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("driver", &self.driver)
            .field("username", &self.username)
            .field("has_password", &self.password.is_some())
            .field("onvif_endpoint", &self.onvif_endpoint)
            .finish()
    }
}

impl SpeakerConfig {
    /// The checks a create or update must pass; the error names the field.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        let host = self.host.as_str();
        let hostname = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            });
        if !hostname && host.parse::<std::net::IpAddr>().is_err() {
            return Err(format!(
                "host must be an IP address or a hostname (no scheme, port, path or spaces), got {host:?}"
            ));
        }
        if self.port == 0 {
            return Err("port must not be 0".into());
        }
        // The SIP driver quotes the username in its Digest header line, where
        // a trailing `\` would escape the closing quote. The password only
        // feeds the MD5 digest.
        let username = self.username.as_deref().unwrap_or_default();
        if username
            .chars()
            .any(|c| c.is_control() || c == '"' || c == '\\')
        {
            return Err(
                "username must not contain a control character, a double quote or a backslash"
                    .into(),
            );
        }
        let password = self.password.as_deref().unwrap_or_default();
        if password.chars().any(char::is_control) {
            return Err("password must not contain a control character".into());
        }
        if let Some(endpoint) = &self.onvif_endpoint {
            self.validate_onvif_endpoint(endpoint)?;
        }
        Ok(())
    }

    /// The endpoint receives the speaker's credentials, so it must be http(s)
    /// on the speaker's own host (no pivoting to another device) and carry no
    /// credentials of its own.
    fn validate_onvif_endpoint(&self, endpoint: &str) -> Result<(), String> {
        let bad = |why: &str| Err(format!("onvif_endpoint {why}"));
        let Ok(url) = url::Url::parse(endpoint) else {
            return bad("must be a URL");
        };
        if !matches!(url.scheme(), "http" | "https") {
            return bad("must be an http or https URL");
        }
        if !url.username().is_empty() || url.password().is_some() {
            return bad("must not carry credentials");
        }
        let url_host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
        let same = url_host.eq_ignore_ascii_case(&self.host)
            || matches!(
                (url_host.parse::<std::net::IpAddr>(), self.host.parse::<std::net::IpAddr>()),
                (Ok(a), Ok(b)) if a == b
            );
        if !same {
            return bad("must be on the speaker's host");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speaker(host: &str) -> SpeakerConfig {
        serde_json::from_value(serde_json::json!({ "name": "lobby", "host": host })).unwrap()
    }

    #[test]
    fn defaults_and_wire_shape() {
        let sp = speaker("192.0.2.10");
        assert_eq!((sp.port, sp.driver), (5060, SpeakerDriver::Sip));
        let json = serde_json::to_value(&sp).unwrap();
        assert_eq!(json["driver"], "sip");
        assert!(json.get("password").is_none());
    }

    #[test]
    fn validate_accepts_addresses_and_hostnames_only() {
        for ok in ["192.0.2.10", "fe80::1", "speaker-1.lan", "dw-swsp16"] {
            assert_eq!(speaker(ok).validate(), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "sip://192.0.2.10",
            "192.0.2.10:5060",
            "host/path",
            "two words",
            "a..b",
        ] {
            assert!(speaker(bad).validate().is_err(), "{bad:?}");
        }
        let mut sp = speaker("192.0.2.10");
        sp.name = " ".into();
        assert!(sp.validate().is_err());
        sp.name = "lobby".into();
        sp.port = 0;
        assert!(sp.validate().is_err());
    }

    #[test]
    fn validate_rejects_credentials_that_would_break_a_sip_header() {
        for bad in ["a\r\nVia: x", "a\nb", "a\u{7f}", "a\"b", "a\\"] {
            let mut sp = speaker("192.0.2.10");
            sp.username = Some(bad.into());
            let err = sp.validate().unwrap_err();
            assert!(err.contains("username"), "{bad:?}: {err}");
        }
        for bad in ["a\r\nVia: x", "a\nb", "a\u{7f}"] {
            let mut sp = speaker("192.0.2.10");
            sp.password = Some(bad.into());
            let err = sp.validate().unwrap_err();
            assert!(err.contains("password"), "{bad:?}: {err}");
        }
        let mut sp = speaker("192.0.2.10");
        sp.username = Some("admin".into());
        sp.password = Some(String::new());
        assert_eq!(sp.validate(), Ok(()));
        // The password only feeds the MD5 digest, never the header line.
        sp.password = Some(r#"p@ss "w0rd"\!"#.into());
        assert_eq!(sp.validate(), Ok(()));
    }

    #[test]
    fn validate_confines_the_onvif_endpoint_to_the_speakers_host() {
        let with = |host: &str, endpoint: &str| {
            let mut sp = speaker(host);
            sp.onvif_endpoint = Some(endpoint.into());
            sp.validate()
        };
        for (host, ok) in [
            ("192.0.2.10", "http://192.0.2.10/onvif/media_service"),
            ("192.0.2.10", "https://192.0.2.10:8443/onvif/media"),
            ("Speaker-1.lan", "http://speaker-1.LAN:8080/onvif"),
            ("fe80::1", "http://[fe80::1]/onvif"),
            ("fe80::1", "http://[fe80:0:0:0:0:0:0:1]/onvif"),
        ] {
            assert_eq!(with(host, ok), Ok(()), "{ok}");
        }
        for (host, bad) in [
            ("192.0.2.10", "http://192.0.2.11/onvif"),
            ("192.0.2.10", "http://192.0.2.10.evil.example/onvif"),
            ("192.0.2.10", "http://admin:pw@192.0.2.10/onvif"),
            ("192.0.2.10", "http://evil.example@192.0.2.10/onvif"),
            ("192.0.2.10", "ftp://192.0.2.10/onvif"),
            ("192.0.2.10", "file:///etc/passwd"),
            ("192.0.2.10", "192.0.2.10/onvif"),
            ("192.0.2.10", ""),
        ] {
            let err = with(host, bad).unwrap_err();
            assert!(err.contains("onvif_endpoint"), "{bad:?}: {err}");
            assert!(!err.contains("pw"), "{err}");
        }
        let mut sp = speaker("192.0.2.10");
        sp.onvif_endpoint = Some("http://192.0.2.10/onvif".into());
        let json = serde_json::to_value(&sp).unwrap();
        assert_eq!(json["onvif_endpoint"], "http://192.0.2.10/onvif");
        assert!(serde_json::to_value(speaker("192.0.2.10"))
            .unwrap()
            .get("onvif_endpoint")
            .is_none());
    }

    #[test]
    fn debug_never_prints_the_password() {
        let mut sp = speaker("192.0.2.10");
        sp.password = Some("hunter2".into());
        assert!(!format!("{sp:?}").contains("hunter2"));
    }
}
