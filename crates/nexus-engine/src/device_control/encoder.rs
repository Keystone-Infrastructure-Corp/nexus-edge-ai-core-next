//! ONVIF device-control HTTP handlers: encoder.

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use nexus_store::audit::AuditOutcome;
use nexus_types::{CameraId, Role};
use serde::{Deserialize, Serialize};
use url::Url;

use super::{audit, finish_void, gateway_err, onvif_target, rbac, OnvifTarget};
use crate::api::{ApiError, ApiState};
use crate::auth::require_role::SessionContext;
use crate::discovery::onvif_encoder::{self, VideoEncoderConfig, VideoEncoderOptions};
use crate::discovery::onvif_media::{self, ProfileStream};

/// Max length of encoder error text that leaves the edge.
const ERR_CAP: usize = 200;

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Total budget for the profile-binding read (GetProfiles + one
/// GetStreamUri per profile). The GET already spends up to two 5 s
/// `REQ_TIMEOUT`s on configs + options and edge-gateway answers the
/// cloud's RPC after 15 s, so 4 s keeps the worst case at 14 s; a
/// healthy camera answers the whole lookup in well under a second.
const BINDING_BUDGET: Duration = Duration::from_secs(4);

/// Budget for the PUT's pre-write reads (configs + selected options).
/// With the 5 s write after it the PUT stays inside edge-gateway's 15 s
/// RPC window, so a slow camera fails clean (nothing written) instead
/// of leaving the cloud an ambiguous write outcome.
const PUT_READ_BUDGET: Duration = Duration::from_secs(6);

#[derive(Deserialize)]
pub struct EncoderQuery {
    /// Whose option ranges to return; absent keeps the generic read.
    configuration_token: Option<String>,
}

/// `GET /v1/admin/cameras/{id}/encoder` — encoder configs, the
/// option ranges of the selected configuration (generic when none
/// is selected; best-effort), and which configuration feeds the
/// recording stream (`bindings`, best-effort, `null` with a
/// `bindings_reason` when the lookup could not run).
pub async fn encoder_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    Query(q): Query<EncoderQuery>,
    ctx: SessionContext,
) -> Result<Json<serde_json::Value>, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let t = onvif_target(&s, id).await?;
    let configs =
        onvif_encoder::get_video_encoder_configurations(&t.endpoint, &t.username, &t.password)
            .await
            .map_err(enc_err)?;
    let selected = q.configuration_token.as_deref();
    if selected.is_some_and(|tok| !configs.iter().any(|c| c.token == tok)) {
        return Err(unknown_token());
    }
    let options = onvif_encoder::get_video_encoder_configuration_options(
        &t.endpoint,
        &t.username,
        &t.password,
        selected,
        None,
    )
    .await
    .ok();
    let recording = recording_url(&s, id).await?;
    let (bindings, bindings_reason) =
        recording_bindings(&t, &configs, &recording, BINDING_BUDGET).await;
    Ok(Json(serde_json::json!({
        "configs": configs,
        "options": options,
        "bindings": bindings,
        "bindings_reason": bindings_reason,
    })))
}

#[derive(Deserialize)]
pub struct EncoderPut {
    config: VideoEncoderConfig,
    #[serde(default)]
    force_persistence: bool,
}

/// `PUT /v1/admin/cameras/{id}/encoder` — write one encoder config.
/// The token must be one the camera lists and every changed value must
/// fit *that* configuration's options; otherwise nothing is written.
pub async fn encoder_put(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(req): Json<EncoderPut>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    const ACTION: &str = "camera.onvif.encoder.set";
    let t = onvif_target(&s, id).await?;
    let checked = tokio::time::timeout(PUT_READ_BUDGET, check_selected(&t, &req.config))
        .await
        .unwrap_or_else(|_| Err(enc_err("encoder read timed out; nothing written".into())));
    if let Err(ApiError(status, e)) = checked {
        audit(
            &s,
            &ctx,
            &headers,
            peer.ip(),
            ACTION,
            id,
            AuditOutcome::Failure,
            Some(&e),
        )
        .await;
        return Err(ApiError(status, e));
    }
    let res = set_config(&t, &req.config, req.force_persistence).await;
    finish_void(&s, &ctx, &headers, peer.ip(), ACTION, id, res).await
}

async fn set_config(t: &OnvifTarget, c: &VideoEncoderConfig, force: bool) -> Result<(), String> {
    onvif_encoder::set_video_encoder_configuration(&t.endpoint, &t.username, &t.password, c, force)
        .await
        .map_err(|e| sanitize_err(&e))
}

/// Camera error text reaches the HTTP response and the audit log, so no
/// URL (a fault can echo a credential-bearing stream URI, R5b) and at most
/// [`ERR_CAP`] chars leave the edge.
fn sanitize_err(e: &str) -> String {
    let words: Vec<&str> = e
        .split_whitespace()
        .map(|w| {
            if w.contains("://") || w.contains('@') {
                "[redacted]"
            } else {
                w
            }
        })
        .collect();
    words.join(" ").chars().take(ERR_CAP).collect()
}

fn enc_err(e: String) -> ApiError {
    gateway_err(sanitize_err(&e))
}

fn unknown_token() -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "unknown encoder configuration token".into(),
    )
}

/// Read the camera's current config for `new.token` and that
/// configuration's options, then [`check_write`].
async fn check_selected(t: &OnvifTarget, new: &VideoEncoderConfig) -> Result<(), ApiError> {
    let configs =
        onvif_encoder::get_video_encoder_configurations(&t.endpoint, &t.username, &t.password)
            .await
            .map_err(enc_err)?;
    let current = configs
        .iter()
        .find(|c| c.token == new.token)
        .ok_or_else(unknown_token)?;
    let options = onvif_encoder::get_video_encoder_configuration_options(
        &t.endpoint,
        &t.username,
        &t.password,
        Some(&new.token),
        None,
    )
    .await
    .map_err(enc_err)?;
    check_write(new, current, &options).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))
}

/// Every field the write changes must fit the selected configuration's
/// options. Unchanged fields are the camera's own values and are not
/// re-judged; a range the camera does not report is left to the camera.
fn check_write(
    new: &VideoEncoderConfig,
    current: &VideoEncoderConfig,
    options: &VideoEncoderOptions,
) -> Result<(), String> {
    fn int(
        name: &str,
        new: Option<u32>,
        cur: Option<u32>,
        r: Option<onvif_encoder::IntRange>,
    ) -> Result<(), String> {
        match (new, r) {
            (Some(v), Some(r))
                if new != cur && !(r.min as i64..=r.max as i64).contains(&(v as i64)) =>
            {
                Err(format!(
                    "{name} {v} outside {}..={} for this configuration",
                    r.min, r.max
                ))
            }
            _ => Ok(()),
        }
    }
    int(
        "bitrate_limit",
        new.bitrate_limit,
        current.bitrate_limit,
        options.bitrate_range,
    )?;
    int(
        "frame_rate_limit",
        new.frame_rate_limit,
        current.frame_rate_limit,
        options.frame_rate_range,
    )?;
    int(
        "gov_length",
        new.gov_length,
        current.gov_length,
        options.gov_length_range,
    )?;
    if let (Some(q), Some(r)) = (new.quality, options.quality_range) {
        if new.quality != current.quality && !(r.min..=r.max).contains(&q) {
            return Err(format!(
                "quality {q} outside {}..={} for this configuration",
                r.min, r.max
            ));
        }
    }
    if let (Some(w), Some(h)) = (new.width, new.height) {
        let changed = (new.width, new.height) != (current.width, current.height);
        if changed && !options.resolutions.is_empty() && !options.resolutions.contains(&(w, h)) {
            return Err(format!(
                "resolution {w}x{h} not offered for this configuration"
            ));
        }
    }
    if let Some(p) = &new.profile {
        if new.profile != current.profile
            && !options.profiles_supported.is_empty()
            && !options.profiles_supported.contains(p)
        {
            return Err(format!("profile {p} not offered for this configuration"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Recording binding (SPEC-090)
// ---------------------------------------------------------------------------

/// One encoder configuration, the media profiles that stream it, and
/// whether it verifiably produces the camera's recording stream
/// (`None` = unverified, never a guess).
#[derive(Debug, PartialEq, Serialize)]
struct Binding {
    configuration_token: String,
    profile_tokens: Vec<String>,
    used_for_recording: Option<bool>,
}

/// The camera's recording stream: its main ingest URL, never the
/// analysis substream.
async fn recording_url(s: &ApiState, id: CameraId) -> Result<Url, ApiError> {
    s.store
        .list_cameras()
        .await?
        .into_iter()
        .find(|c| c.id == id)
        .map(|c| c.ingest.url)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "camera not found".into()))
}

/// Resolve the camera's profiles within `budget` and bind them. On
/// budget exhaustion or a failed GetProfiles, `bindings` is `None`
/// with a reason code; the caller still returns configs/options.
async fn recording_bindings(
    t: &OnvifTarget,
    configs: &[VideoEncoderConfig],
    recording: &Url,
    budget: Duration,
) -> (Option<Vec<Binding>>, Option<&'static str>) {
    let lookup = onvif_media::query_profile_streams(&t.endpoint, &t.username, &t.password);
    match tokio::time::timeout(budget, lookup).await {
        Err(_) => (None, Some("binding_lookup_timeout")),
        Ok(Err(_)) => (None, Some("profiles_unavailable")),
        Ok(Ok(profiles)) => {
            let (bindings, reason) = bind_recording(configs, &profiles, recording);
            (Some(bindings), reason)
        }
    }
}

/// How one camera-reported stream URI relates to the recording URL.
#[derive(PartialEq)]
enum UriVerdict {
    Same,
    Different,
    /// Same path+query under another authority (a vendor host/port
    /// alias we cannot prove equal), or a URI that does not parse.
    Unverified,
}

/// Comparison key: (scheme, lower-cased host, port with RTSP's 554
/// filled in) and (path without trailing slash, non-empty query).
/// Userinfo is dropped; path and query compare exactly.
type UriKey<'a> = ((&'a str, String, Option<u16>), (&'a str, Option<&'a str>));

fn uri_key(u: &Url) -> UriKey<'_> {
    let port = u.port().or((u.scheme() == "rtsp").then_some(554));
    let host = u.host_str().unwrap_or_default().to_ascii_lowercase();
    let query = u.query().filter(|q| !q.is_empty());
    (
        (u.scheme(), host, port),
        (u.path().trim_end_matches('/'), query),
    )
}

fn compare_uri(reported: &str, recording: &Url) -> UriVerdict {
    let Ok(reported) = Url::parse(reported) else {
        return UriVerdict::Unverified;
    };
    let (a, b) = (uri_key(&reported), uri_key(recording));
    if a == b {
        UriVerdict::Same
    } else if a.1 == b.1 {
        UriVerdict::Unverified
    } else {
        UriVerdict::Different
    }
}

/// Bind each configuration to its profiles and decide which one feeds
/// recording. One configuration is `true` only when every profile's
/// stream URI resolved and all exact matches belong to that one listed
/// configuration; the rest are then `false`. Anything else leaves every
/// configuration `null` with the reason, because an unresolved profile
/// could be a second match. No match at all is `null` too: the stored
/// URL may be a vendor path alias, so "none of these" is unproven.
fn bind_recording(
    configs: &[VideoEncoderConfig],
    profiles: &[ProfileStream],
    recording: &Url,
) -> (Vec<Binding>, Option<&'static str>) {
    let verdicts: Vec<Option<UriVerdict>> = profiles
        .iter()
        .map(|p| p.uri.as_deref().map(|u| compare_uri(u, recording)))
        .collect();
    let mut matched: Vec<Option<&str>> = profiles
        .iter()
        .zip(&verdicts)
        .filter(|(_, v)| **v == Some(UriVerdict::Same))
        .map(|(p, _)| p.encoder_token.as_deref())
        .collect();
    matched.sort_unstable();
    matched.dedup();
    let reason = if verdicts.iter().any(Option::is_none) {
        Some("stream_uri_unavailable")
    } else if verdicts.contains(&Some(UriVerdict::Unverified)) {
        Some("stream_uri_unverified")
    } else if matched.is_empty() {
        Some("no_recording_match")
    } else if matched.len() > 1 {
        Some("multiple_configs_match")
    } else if !configs.iter().any(|c| Some(c.token.as_str()) == matched[0]) {
        Some("recording_profile_unbound")
    } else {
        None
    };
    let bindings = configs
        .iter()
        .map(|c| Binding {
            configuration_token: c.token.clone(),
            profile_tokens: profiles
                .iter()
                .filter(|p| p.encoder_token.as_deref() == Some(c.token.as_str()))
                .map(|p| p.profile_token.clone())
                .collect(),
            used_for_recording: reason
                .is_none()
                .then(|| matched[0] == Some(c.token.as_str())),
        })
        .collect();
    (bindings, reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::onvif_encoder::IntRange;

    fn cfg(token: &str) -> VideoEncoderConfig {
        VideoEncoderConfig {
            token: token.into(),
            ..VideoEncoderConfig::default()
        }
    }

    fn prof(token: &str, encoder: Option<&str>, uri: Option<&str>) -> ProfileStream {
        ProfileStream {
            profile_token: token.into(),
            encoder_token: encoder.map(str::to_string),
            uri: uri.map(str::to_string),
        }
    }

    fn rec(u: &str) -> url::Url {
        u.parse().unwrap()
    }

    fn b(token: &str, profiles: &[&str], used: Option<bool>) -> Binding {
        Binding {
            configuration_token: token.into(),
            profile_tokens: profiles.iter().map(|p| p.to_string()).collect(),
            used_for_recording: used,
        }
    }

    const MAIN: &str = "rtsp://192.168.1.64:554/Streaming/Channels/101";
    const SUB: &str = "rtsp://192.168.1.64:554/Streaming/Channels/102";

    #[test]
    fn shared_config_across_two_profiles_is_the_verified_recording_encoder() {
        let configs = [cfg("VE_main"), cfg("VE_sub"), cfg("VE_unused")];
        let profiles = [
            prof("P1", Some("VE_main"), Some(MAIN)),
            prof("P2", Some("VE_sub"), Some(SUB)),
            prof("P3", Some("VE_sub"), Some(SUB)),
        ];
        let (bindings, reason) = bind_recording(
            &configs,
            &profiles,
            &rec("rtsp://192.168.1.64:554/Streaming/Channels/102"),
        );
        assert_eq!(reason, None);
        assert_eq!(
            bindings,
            vec![
                b("VE_main", &["P1"], Some(false)),
                b("VE_sub", &["P2", "P3"], Some(true)),
                b("VE_unused", &[], Some(false)),
            ]
        );
    }

    #[test]
    fn implicit_port_credentials_case_and_trailing_slash_normalise_away() {
        let configs = [cfg("VE_main"), cfg("VE_sub")];
        let profiles = [
            prof("P1", Some("VE_main"), Some(MAIN)),
            prof(
                "P2",
                Some("VE_sub"),
                Some("RTSP://User:Pw@192.168.1.64/Streaming/Channels/102/"),
            ),
        ];
        let (bindings, reason) = bind_recording(
            &configs,
            &profiles,
            &rec("rtsp://admin:secret@192.168.1.64/Streaming/Channels/102"),
        );
        assert_eq!(reason, None);
        assert_eq!(bindings[1].used_for_recording, Some(true));
        assert_eq!(bindings[0].used_for_recording, Some(false));
        // An explicit non-default port is a different authority, not a match.
        let (bindings, reason) = bind_recording(
            &configs,
            &profiles,
            &rec("rtsp://192.168.1.64:8554/Streaming/Channels/102"),
        );
        assert_eq!(reason, Some("stream_uri_unverified"));
        assert!(bindings.iter().all(|b| b.used_for_recording.is_none()));
    }

    #[test]
    fn path_and_query_compare_exactly() {
        let configs = [cfg("VE_main")];
        let profiles = [prof(
            "P1",
            Some("VE_main"),
            Some("rtsp://cam/Streaming/Channels/101?transportmode=unicast"),
        )];
        let (bindings, reason) = bind_recording(
            &configs,
            &profiles,
            &rec("rtsp://cam/streaming/channels/101"),
        );
        assert_eq!(reason, Some("no_recording_match"));
        assert_eq!(bindings[0].used_for_recording, None);
    }

    #[test]
    fn vendor_alias_host_is_unverified_not_guessed() {
        let configs = [cfg("VE_main"), cfg("VE_sub")];
        let profiles = [
            prof("P1", Some("VE_main"), Some(MAIN)),
            prof(
                "P2",
                Some("VE_sub"),
                Some("rtsp://cam-front.local:554/Streaming/Channels/102"),
            ),
        ];
        let (bindings, reason) = bind_recording(&configs, &profiles, &rec(SUB));
        assert_eq!(reason, Some("stream_uri_unverified"));
        assert!(bindings.iter().all(|b| b.used_for_recording.is_none()));
    }

    #[test]
    fn two_configs_matching_is_ambiguous() {
        let configs = [cfg("VE_main"), cfg("VE_sub")];
        let profiles = [
            prof("P1", Some("VE_main"), Some(SUB)),
            prof("P2", Some("VE_sub"), Some(SUB)),
        ];
        let (bindings, reason) = bind_recording(&configs, &profiles, &rec(SUB));
        assert_eq!(reason, Some("multiple_configs_match"));
        assert!(bindings.iter().all(|b| b.used_for_recording.is_none()));
    }

    #[test]
    fn a_failed_stream_uri_leaves_every_config_unverified() {
        let configs = [cfg("VE_main"), cfg("VE_sub")];
        let profiles = [
            prof("P1", Some("VE_main"), None),
            prof("P2", Some("VE_sub"), Some(SUB)),
        ];
        let (bindings, reason) = bind_recording(&configs, &profiles, &rec(SUB));
        assert_eq!(reason, Some("stream_uri_unavailable"));
        assert_eq!(
            bindings,
            vec![b("VE_main", &["P1"], None), b("VE_sub", &["P2"], None)]
        );
    }

    #[test]
    fn no_match_anywhere_is_unverified_not_false() {
        let configs = [cfg("VE_main"), cfg("VE_sub")];
        let profiles = [
            prof("P1", Some("VE_main"), Some(MAIN)),
            prof("P2", Some("VE_sub"), Some(SUB)),
        ];
        let (bindings, reason) = bind_recording(
            &configs,
            &profiles,
            &rec("rtsp://192.168.1.64/h264/ch1/main/av_stream"),
        );
        assert_eq!(reason, Some("no_recording_match"));
        assert!(bindings.iter().all(|b| b.used_for_recording.is_none()));
    }

    #[test]
    fn a_matching_profile_without_an_encoder_config_is_unverified() {
        let configs = [cfg("VE_main")];
        let profiles = [
            prof("P1", Some("VE_main"), Some(MAIN)),
            prof("P2", None, Some(SUB)),
        ];
        let (bindings, reason) = bind_recording(&configs, &profiles, &rec(SUB));
        assert_eq!(reason, Some("recording_profile_unbound"));
        assert_eq!(bindings, vec![b("VE_main", &["P1"], None)]);
    }

    const LEAK: &str = "rtsp://user:pass@10.0.0.9:554/s?token=abc http://user:pass@cam/x";

    /// Camera stub: valid configs, SOAP faults (echoing credential-bearing
    /// URIs) for options, profiles and set.
    async fn fault_camera() -> OnvifTarget {
        async fn h(body: String) -> (StatusCode, String) {
            if body.contains("GetVideoEncoderConfigurations") {
                return (
                    StatusCode::OK,
                    r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><s:Body><trt:GetVideoEncoderConfigurationsResponse><trt:Configurations token="VE"><tt:Name>m</tt:Name></trt:Configurations></trt:GetVideoEncoderConfigurationsResponse></s:Body></s:Envelope>"#.into(),
                );
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Code><s:Value>s:Receiver</s:Value></s:Code><s:Reason><s:Text xml:lang="en">bad {LEAK} {}</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>"#,
                    "x".repeat(500)
                ),
            )
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, axum::Router::new().fallback(h)).await;
        });
        OnvifTarget {
            endpoint: format!("http://{addr}/onvif/device_service"),
            username: "admin".into(),
            password: "secret".into(),
        }
    }

    fn assert_clean(text: &str) {
        assert!(!text.contains("user:pass@"), "{text}");
        assert!(!text.contains("://"), "{text}");
        assert!(!text.contains("token=abc"), "{text}");
        assert!(text.chars().count() <= ERR_CAP, "{text}");
        assert!(text.contains("soap fault"), "keeps a stable reason: {text}");
    }

    #[tokio::test]
    async fn options_fault_with_credential_uri_is_sanitized() {
        let t = fault_camera().await;
        let ApiError(_, e) = check_selected(&t, &cfg("VE")).await.unwrap_err();
        assert_clean(&e);
    }

    #[tokio::test]
    async fn set_fault_with_credential_uri_is_sanitized() {
        let t = fault_camera().await;
        let e = set_config(&t, &cfg("VE"), false).await.unwrap_err();
        assert_clean(&e);
    }

    #[tokio::test]
    async fn binding_read_fault_surfaces_only_a_reason_code() {
        let t = fault_camera().await;
        let (b, reason) =
            recording_bindings(&t, &[cfg("VE")], &rec(MAIN), Duration::from_secs(4)).await;
        assert!(b.is_none());
        assert_eq!(reason, Some("profiles_unavailable"));
    }

    #[tokio::test]
    async fn binding_lookup_past_its_budget_returns_null_with_a_reason() {
        async fn slow(_body: String) -> &'static str {
            tokio::time::sleep(Duration::from_secs(3)).await;
            "<late/>"
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, axum::Router::new().fallback(slow)).await;
        });
        let t = OnvifTarget {
            endpoint: format!("http://{addr}/onvif/device_service"),
            username: "admin".into(),
            password: "secret".into(),
        };
        let started = std::time::Instant::now();
        let got = recording_bindings(
            &t,
            &[cfg("VE_main")],
            &rec(MAIN),
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(got, (None, Some("binding_lookup_timeout")));
        assert!(started.elapsed() < Duration::from_secs(2));

        let dead = OnvifTarget {
            endpoint: "http://127.0.0.1:9/onvif/device_service".into(),
            username: "admin".into(),
            password: "secret".into(),
        };
        let got =
            recording_bindings(&dead, &[cfg("VE_main")], &rec(MAIN), Duration::from_secs(4)).await;
        assert_eq!(got, (None, Some("profiles_unavailable")));
    }

    fn sub_options() -> VideoEncoderOptions {
        VideoEncoderOptions {
            resolutions: vec![(1280, 720), (640, 360)],
            bitrate_range: Some(IntRange { min: 32, max: 4096 }),
            frame_rate_range: Some(IntRange { min: 1, max: 25 }),
            gov_length_range: Some(IntRange { min: 1, max: 200 }),
            ..VideoEncoderOptions::default()
        }
    }

    fn sub_config() -> VideoEncoderConfig {
        VideoEncoderConfig {
            token: "VE_sub".into(),
            encoding: Some("H264".into()),
            width: Some(1280),
            height: Some(720),
            frame_rate_limit: Some(15),
            bitrate_limit: Some(2048),
            gov_length: Some(30),
            // A value the camera's own options do not list.
            quality: Some(9.0),
            ..VideoEncoderConfig::default()
        }
    }

    #[test]
    fn write_check_validates_changed_fields_against_the_selected_options() {
        let current = sub_config();
        let opts = sub_options();
        let ok = VideoEncoderConfig {
            bitrate_limit: Some(1536),
            ..current.clone()
        };
        assert_eq!(check_write(&ok, &current, &opts), Ok(()));
        for bad in [
            VideoEncoderConfig {
                bitrate_limit: Some(8000),
                ..current.clone()
            },
            VideoEncoderConfig {
                width: Some(2592),
                height: Some(1520),
                ..current.clone()
            },
            VideoEncoderConfig {
                frame_rate_limit: Some(30),
                ..current.clone()
            },
            VideoEncoderConfig {
                gov_length: Some(400),
                ..current.clone()
            },
        ] {
            assert!(check_write(&bad, &current, &opts).is_err(), "{bad:?}");
        }
    }
}
