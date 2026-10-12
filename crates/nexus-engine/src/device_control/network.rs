//! ONVIF device-control HTTP handlers: network-settings.

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nexus_bus::{topic, BusExt};
use nexus_store::audit::AuditOutcome;
use nexus_types::{CameraId, Role};
use serde::Deserialize;

use super::{audit, gateway_err, rbac, OnvifTarget};
use crate::api::{ApiError, ApiState};
use crate::auth::require_role::SessionContext;
use crate::discovery::{onvif_device, onvif_network};

// ---------------------------------------------------------------------------
// Network settings (Phase 7.6.9)
// ---------------------------------------------------------------------------

/// How many times the dangerous-write re-probe pings the camera at
/// its new address before giving up and failing loud. A static-IP
/// change that needs no reboot answers on the first attempt; a change
/// that reboots the camera frequently won't answer within this window
/// — and per the 7.6.9 hazard note we then surface a **loud** failure
/// (the operator may need to physically reach the camera) rather than
/// a false success, because there is no transactional ONVIF revert.
const REPROBE_ATTEMPTS: u32 = 5;

/// Delay between re-probe attempts.
const REPROBE_INTERVAL: Duration = Duration::from_millis(500);

/// Load the full [`nexus_config::CameraConfig`] (the lockstep
/// endpoint-rewrite handlers mutate the whole struct, not just the
/// ONVIF target).
async fn load_camera(s: &ApiState, id: CameraId) -> Result<nexus_config::CameraConfig, ApiError> {
    s.store
        .list_cameras()
        .await?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "camera not found".into()))
}

/// Resolve the ONVIF target straight off an already-loaded camera
/// (avoids a second store round-trip).
fn target_from(cam: &nexus_config::CameraConfig) -> Result<OnvifTarget, ApiError> {
    let endpoint = cam
        .onvif
        .endpoint
        .clone()
        .filter(|e| !e.trim().is_empty())
        .ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "camera has no ONVIF endpoint configured".into(),
            )
        })?;
    Ok(OnvifTarget {
        endpoint,
        username: cam.onvif.username.clone().unwrap_or_default(),
        password: cam.onvif.password.clone().unwrap_or_default(),
    })
}

/// Persist a rewritten camera config and signal the hot-reload
/// reconciler to respawn the camera source at its new address — the
/// "respawn the camera source" half of the lockstep safety net.
async fn persist_and_respawn(
    s: &ApiState,
    cam: &nexus_config::CameraConfig,
) -> Result<(), ApiError> {
    s.store.upsert_camera(cam).await?;
    let _ = s
        .bus
        .publish(
            topic::CONFIG_CHANGED,
            &serde_json::json!({ "kind": "camera", "action": "upsert", "camera_id": cam.id }),
        )
        .await;
    Ok(())
}

/// Re-probe a camera at the (rewritten) ONVIF endpoint until it
/// answers a lightweight `GetSystemDateAndTime` or the attempts run
/// out. `Ok` means the camera is reachable at its new address.
async fn reprobe(endpoint: &str, username: &str, password: &str) -> Result<(), String> {
    for attempt in 0..REPROBE_ATTEMPTS {
        if onvif_device::get_system_date_and_time(endpoint, username, password)
            .await
            .is_ok()
        {
            return Ok(());
        }
        if attempt + 1 < REPROBE_ATTEMPTS {
            tokio::time::sleep(REPROBE_INTERVAL).await;
        }
    }
    Err("camera did not answer at its new address".into())
}

/// Rewrite the stored `ingest.url` + `onvif_endpoint` host to
/// `new_host` (an IP literal or DNS name) in lockstep — the dangerous
/// IP-change safety net. Mutates `cam` in place.
fn rewrite_camera_host(cam: &mut nexus_config::CameraConfig, new_host: &str) -> Result<(), String> {
    onvif_network::set_url_host(&mut cam.ingest.url, new_host)?;
    if let Some(ep) = cam.onvif.endpoint.as_deref() {
        cam.onvif.endpoint = Some(onvif_network::endpoint_with_host(ep, new_host)?);
    }
    Ok(())
}

/// Rewrite the stored `ingest.url` (RTSP) / `onvif_endpoint`
/// (HTTP/HTTPS) port in lockstep with a `SetNetworkProtocols` port
/// change. Returns `true` when anything changed. The camera stays at
/// the same IP, so this is the "safe" half — no re-probe needed, but
/// the source still respawns onto the new port.
fn rewrite_camera_ports(
    cam: &mut nexus_config::CameraConfig,
    protocols: &[NetworkProtocolBody],
) -> bool {
    let mut changed = false;
    for p in protocols {
        let Some(port) = p.port else { continue };
        match p.name.to_ascii_uppercase().as_str() {
            "RTSP" => {
                let scheme = cam.ingest.url.scheme();
                let is_rtsp = scheme == "rtsp" || scheme == "rtsps";
                if is_rtsp
                    && cam.ingest.url.port() != Some(port)
                    && cam.ingest.url.set_port(Some(port)).is_ok()
                {
                    changed = true;
                }
            }
            name @ ("HTTP" | "HTTPS") => {
                let want = if name == "HTTP" { "http" } else { "https" };
                let Some(ep) = cam.onvif.endpoint.as_deref() else {
                    continue;
                };
                let Ok(u) = url::Url::parse(ep) else { continue };
                if u.scheme() != want || u.port() == Some(port) {
                    continue;
                }
                if let Ok(newep) = onvif_network::endpoint_with_port(ep, port) {
                    cam.onvif.endpoint = Some(newep);
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

/// `GET /v1/admin/cameras/{id}/network` — read the full network
/// posture (always safe). Every sub-read is best-effort: a camera
/// that doesn't implement one operation (vendor-spotty
/// `GetZeroConfiguration`, say) yields `null` for that section
/// instead of failing the whole read. NTP is folded in from the core
/// device service. Not audited (a read).
pub async fn network_get(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    ctx: SessionContext,
) -> Result<Json<serde_json::Value>, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let cam = load_camera(&s, id).await?;
    let t = target_from(&cam)?;
    let (ep, u, p) = (
        t.endpoint.as_str(),
        t.username.as_str(),
        t.password.as_str(),
    );

    let interfaces = onvif_network::get_network_interfaces(ep, u, p).await.ok();
    let default_gateway = onvif_network::get_network_default_gateway(ep, u, p)
        .await
        .ok();
    let dns = onvif_network::get_dns(ep, u, p).await.ok();
    let ntp = onvif_device::get_ntp(ep, u, p).await.ok();
    let hostname = onvif_network::get_hostname(ep, u, p).await.ok();
    let network_protocols = onvif_network::get_network_protocols(ep, u, p).await.ok();
    let zero_configuration = onvif_network::get_zero_configuration(ep, u, p).await.ok();

    Ok(Json(serde_json::json!({
        "interfaces": interfaces,
        "default_gateway": default_gateway,
        "dns": dns,
        "ntp": ntp,
        "hostname": hostname,
        "network_protocols": network_protocols,
        "zero_configuration": zero_configuration,
    })))
}

/// One protocol toggle in a `set_network_protocols` safe write.
#[derive(Deserialize)]
pub struct NetworkProtocolBody {
    name: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    port: Option<u16>,
}

/// A **safe** network write — NTP / DNS / hostname / protocol-port
/// changes that cannot isolate the camera from the edge. Tagged on
/// `action`.
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // variant names map to the ONVIF Set* action tags
pub enum NetworkSafeRequest {
    SetNtp {
        #[serde(default)]
        from_dhcp: bool,
        #[serde(default)]
        server: Option<String>,
    },
    SetDns {
        #[serde(default)]
        from_dhcp: bool,
        #[serde(default)]
        servers: Vec<String>,
        #[serde(default)]
        search_domain: Option<String>,
    },
    SetHostname {
        name: String,
    },
    SetNetworkProtocols {
        #[serde(default)]
        protocols: Vec<NetworkProtocolBody>,
    },
}

/// `PUT /v1/admin/cameras/{id}/network/safe` — apply a safe network
/// write. A `set_network_protocols` port change additionally rewrites
/// the stored `ingest.url` / `onvif_endpoint` port in lockstep and
/// respawns the source. Audited.
pub async fn network_safe_put(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(req): Json<NetworkSafeRequest>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let ip = peer.ip();
    let mut cam = load_camera(&s, id).await?;
    let t = target_from(&cam)?;

    let (action, detail, res): (&str, String, Result<(), String>) = match &req {
        NetworkSafeRequest::SetNtp { from_dhcp, server } => (
            "camera.onvif.network.ntp",
            format!("from_dhcp={from_dhcp}"),
            onvif_device::set_ntp(
                &t.endpoint,
                &t.username,
                &t.password,
                *from_dhcp,
                server.as_deref(),
            )
            .await,
        ),
        NetworkSafeRequest::SetDns {
            from_dhcp,
            servers,
            search_domain,
        } => (
            "camera.onvif.network.dns",
            format!("from_dhcp={from_dhcp}; servers={}", servers.len()),
            onvif_network::set_dns(
                &t.endpoint,
                &t.username,
                &t.password,
                *from_dhcp,
                servers,
                search_domain.as_deref(),
            )
            .await,
        ),
        NetworkSafeRequest::SetHostname { name } => (
            "camera.onvif.network.hostname",
            format!("name={name}"),
            onvif_network::set_hostname(&t.endpoint, &t.username, &t.password, name).await,
        ),
        NetworkSafeRequest::SetNetworkProtocols { protocols } => {
            let sets: Vec<onvif_network::NetworkProtocolSet> = protocols
                .iter()
                .map(|p| onvif_network::NetworkProtocolSet {
                    name: p.name.clone(),
                    enabled: p.enabled,
                    port: p.port,
                })
                .collect();
            (
                "camera.onvif.network.protocols",
                format!("protocols={}", protocols.len()),
                onvif_network::set_network_protocols(&t.endpoint, &t.username, &t.password, &sets)
                    .await,
            )
        }
    };

    if let Err(e) = res {
        audit(
            &s,
            &ctx,
            &headers,
            ip,
            action,
            id,
            AuditOutcome::Failure,
            Some(&e),
        )
        .await;
        return Err(gateway_err(e));
    }

    // Lockstep port rewrite for a protocol-port change.
    let mut endpoint_rewritten = false;
    if let NetworkSafeRequest::SetNetworkProtocols { protocols } = &req {
        if rewrite_camera_ports(&mut cam, protocols) {
            persist_and_respawn(&s, &cam).await?;
            endpoint_rewritten = true;
        }
    }

    let detail = if endpoint_rewritten {
        format!("{detail}; endpoint port rewritten in lockstep")
    } else {
        detail
    };
    audit(
        &s,
        &ctx,
        &headers,
        ip,
        action,
        id,
        AuditOutcome::Success,
        Some(&detail),
    )
    .await;
    Ok(Json(serde_json::json!({
        "applied": true,
        "endpoint_rewritten": endpoint_rewritten,
    }))
    .into_response())
}

/// The IPv4 interface reconfiguration in a dangerous network write.
#[derive(Deserialize)]
pub struct NetworkIfaceBody {
    token: String,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    mtu: Option<u32>,
    #[serde(default)]
    dhcp: bool,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    prefix_length: Option<u8>,
}

/// A **dangerous** network write — `SetNetworkInterfaces` and/or
/// `SetNetworkDefaultGateway`, either of which can sever the edge's
/// own ingest path.
#[derive(Deserialize)]
pub struct NetworkInterfaceRequest {
    /// Interface IPv4 reconfiguration. Optional — a gateway-only
    /// change omits it.
    #[serde(default)]
    interface: Option<NetworkIfaceBody>,
    /// New IPv4 default gateway(s). Optional.
    #[serde(default)]
    gateway_ipv4: Option<Vec<String>>,
    /// The host (IP literal or DNS name) the camera will be reachable
    /// at after the change. Drives the lockstep `ingest.url` /
    /// `onvif_endpoint` rewrite and the re-probe. Required.
    new_endpoint_host: String,
}

/// `PUT /v1/admin/cameras/{id}/network/interface` — apply a dangerous
/// network write (IP / DHCP↔static / MTU / default gateway).
///
/// Edge-side this requires the top tier (`Admin`); the **owner-only**
/// restriction and the type-token confirmation are enforced
/// cloud-side. Flow (per the 7.6.9 hazard note): apply to the camera
/// → rewrite the stored `ingest.url` + `onvif_endpoint` to the new
/// address **atomically with the apply** and respawn the source →
/// re-probe at the new address → **fail loud** if the camera does not
/// answer (it may need a physical reset; there is no transactional
/// ONVIF revert). The audit detail is credential-free (only the bare
/// new host, never the credential-bearing `ingest.url`).
pub async fn network_interface_put(
    State(s): State<ApiState>,
    Path(id): Path<CameraId>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ctx: SessionContext,
    Json(req): Json<NetworkInterfaceRequest>,
) -> Result<Response, ApiError> {
    rbac(&ctx, Role::Admin)?;
    let ip = peer.ip();
    const ACTION: &str = "camera.onvif.network.interface";

    let new_host = req.new_endpoint_host.trim().to_string();
    if new_host.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "new_endpoint_host required".into(),
        ));
    }
    let has_gateway = req
        .gateway_ipv4
        .as_ref()
        .is_some_and(|g| g.iter().any(|x| !x.trim().is_empty()));
    if req.interface.is_none() && !has_gateway {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "no interface or gateway change supplied".into(),
        ));
    }

    let mut cam = load_camera(&s, id).await?;
    let t = target_from(&cam)?;

    // 1. Apply the interface reconfiguration (if present).
    let mut reboot_needed = false;
    if let Some(iface) = &req.interface {
        let set = onvif_network::Ipv4InterfaceSet {
            token: iface.token.clone(),
            enabled: iface.enabled,
            mtu: iface.mtu,
            dhcp: iface.dhcp,
            address: iface.address.clone(),
            prefix_length: iface.prefix_length,
        };
        match onvif_network::set_network_interfaces(&t.endpoint, &t.username, &t.password, &set)
            .await
        {
            Ok(rb) => reboot_needed = rb,
            Err(e) => {
                audit(
                    &s,
                    &ctx,
                    &headers,
                    ip,
                    ACTION,
                    id,
                    AuditOutcome::Failure,
                    Some(&e),
                )
                .await;
                return Err(gateway_err(e));
            }
        }
    }

    // 2. Apply the default gateway (if present).
    if let Some(gws) = req
        .gateway_ipv4
        .as_ref()
        .filter(|g| has_gateway && !g.is_empty())
    {
        if let Err(e) =
            onvif_network::set_network_default_gateway(&t.endpoint, &t.username, &t.password, gws)
                .await
        {
            audit(
                &s,
                &ctx,
                &headers,
                ip,
                ACTION,
                id,
                AuditOutcome::Failure,
                Some(&e),
            )
            .await;
            return Err(gateway_err(e));
        }
    }

    // 3. Lockstep rewrite + persist + respawn — atomic with the apply
    //    so the edge keeps pointing at the camera even though its
    //    address just moved.
    if let Err(e) = rewrite_camera_host(&mut cam, &new_host) {
        audit(
            &s,
            &ctx,
            &headers,
            ip,
            ACTION,
            id,
            AuditOutcome::Failure,
            Some(&e),
        )
        .await;
        return Err(ApiError(StatusCode::BAD_REQUEST, e));
    }
    persist_and_respawn(&s, &cam).await?;

    // 4. Re-probe at the new address; fail loud if unreachable.
    let new_t = target_from(&cam)?;
    let reachable = reprobe(&new_t.endpoint, &new_t.username, &new_t.password)
        .await
        .is_ok();
    if !reachable {
        let detail = format!(
            "network change applied; camera UNREACHABLE at new host {new_host} (may need physical reset); reboot_needed={reboot_needed}"
        );
        audit(
            &s,
            &ctx,
            &headers,
            ip,
            ACTION,
            id,
            AuditOutcome::Failure,
            Some(&detail),
        )
        .await;
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            format!(
                "network change applied but camera is unreachable at {new_host} — it may need a physical reset"
            ),
        ));
    }

    let detail =
        format!("network change applied; reachable at {new_host}; reboot_needed={reboot_needed}");
    audit(
        &s,
        &ctx,
        &headers,
        ip,
        ACTION,
        id,
        AuditOutcome::Success,
        Some(&detail),
    )
    .await;
    Ok(Json(serde_json::json!({
        "applied": true,
        "reachable": true,
        "reboot_needed": reboot_needed,
        "new_endpoint_host": new_host,
    }))
    .into_response())
}
