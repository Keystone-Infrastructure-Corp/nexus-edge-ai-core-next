//! OAuth auth-code dance for cloud cold backends: `start`, `callback` and
//! `status` handlers, and the popup HTML response.

use super::*;

// ===========================================================================
// M2.2 closeout — OAuth auth-code dance for cloud cold backends.
//
// The three handlers below replace the previous "register an OAuth
// app in a sibling service and paste the refresh token here"
// step. They run end-to-end inside nexus-engine + the core-next UI:
//
//   POST /api/v1/admin/oauth/{provider}/start       (admin-gated)
//   GET  /api/v1/admin/oauth/{provider}/callback    (state-gated)
//   GET  /api/v1/admin/oauth/status?state=...       (admin-gated)
//
// `start` stashes the form fields in an in-memory cache, returns
// the consent URL the UI must `window.open`. The provider redirects
// the popup to `callback`, which exchanges the auth code for a
// refresh token, encrypts it, and writes the backend row in the
// same shape `put_storage_backend` would have. The UI polls
// `status` until it sees `Complete { handle }`, then reloads its
// backend list.
//
// The OAuth primitives (auth URL builder + code-exchange POST)
// live in `nexus-storage-cloud::oauth`; this module is just the
// HTTP glue + state machine.
// ===========================================================================

#[derive(serde::Deserialize)]
pub(super) struct OAuthStartReq {
    /// Backend handle the resulting `storage_backends` row will
    /// take. Must satisfy `^[a-z0-9][a-z0-9_-]*$` and not be the
    /// reserved name `local`.
    handle: String,
    client_id: String,
    client_secret: String,
    /// Operator-visible label surfaced in the admin UI's "connect
    /// status" string. Free-form but typically the email of the
    /// account that will consent in the popup.
    account_email: Option<String>,
    /// gdrive-only optional knob. Ignored for onedrive.
    root_folder_id: Option<String>,
    /// Where the provider must redirect the browser after consent.
    /// MUST match a redirect URI registered on the OAuth app at
    /// the provider AND must end in the engine's own
    /// `/api/v1/admin/oauth/{provider}/callback` path. The UI
    /// computes this from `location.origin` so dev / staging /
    /// prod all work without a config knob.
    redirect_uri: String,
}

#[derive(serde::Serialize)]
pub(super) struct OAuthStartResp {
    authorize_url: String,
    state: String,
    expires_in_secs: u64,
}

pub(super) async fn start_oauth(
    State(s): State<ApiState>,
    Path(provider_str): Path<String>,
    Json(req): Json<OAuthStartReq>,
) -> Result<Json<OAuthStartResp>, ApiError> {
    // 1. Provider validation. Anything other than the two known
    //    strings is a 404 so callers don't accidentally rely on
    //    the engine to discover provider names.
    let provider = nexus_storage_cloud::Provider::from_kind(&provider_str).ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            format!("unknown OAuth provider '{provider_str}' — supported: gdrive, onedrive"),
        )
    })?;

    // 2. Handle validation. Same regex as `put_storage_backend` so
    //    the caller can't smuggle "../" or `local` past us. We
    //    reject early before stashing in the cache to keep error
    //    surfaces small.
    let handle = req.handle.trim().to_string();
    if !is_valid_handle(&handle) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "handle must match ^[a-z0-9][a-z0-9_-]*$ and not be 'local'".to_string(),
        ));
    }

    // 3. Required-field validation. We don't want to send a
    //    consent URL the operator will only discover is broken
    //    after the popup opens.
    if req.client_id.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "client_id is required".to_string(),
        ));
    }
    if req.client_secret.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "client_secret is required".to_string(),
        ));
    }
    if !req.redirect_uri.ends_with(&format!(
        "/api/v1/admin/oauth/{}/callback",
        provider.as_str()
    )) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "redirect_uri must end with '/api/v1/admin/oauth/{}/callback'",
                provider.as_str()
            ),
        ));
    }
    // Admin-secret presence is a hard precondition: without it the
    // callback can't encrypt the refresh token before persisting.
    // Fail fast in `start` so the operator sees a clean error in
    // the form rather than after consent.
    if s.admin_auth.admin_secret().is_none() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "cloud backend OAuth requires auth.admin_secret_path to be configured \
             (used to encrypt the OAuth refresh token at rest)"
                .to_string(),
        ));
    }

    // 4. Mint state + stash the session.
    let state_token = nexus_storage_cloud::new_state();
    let session = crate::oauth_sessions::PendingSession {
        provider: provider.as_str().to_string(),
        handle: handle.clone(),
        client_id: req.client_id.trim().to_string(),
        client_secret: req.client_secret,
        account_email: req
            .account_email
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty()),
        root_folder_id: req
            .root_folder_id
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty()),
        redirect_uri: req.redirect_uri.clone(),
        created_at: std::time::Instant::now(),
        status: crate::oauth_sessions::SessionStatus::Pending,
    };
    s.oauth_sessions.insert(state_token.clone(), session);

    // 5. Build the consent URL + return.
    let authorize_url = nexus_storage_cloud::authorize_url(
        provider,
        req.client_id.trim(),
        &req.redirect_uri,
        &state_token,
    );

    tracing::info!(
        provider = provider.as_str(),
        handle = %handle,
        "OAuth auth-code flow: started; awaiting callback"
    );

    Ok(Json(OAuthStartResp {
        authorize_url,
        state: state_token,
        expires_in_secs: crate::oauth_sessions::SESSION_TTL.as_secs(),
    }))
}

#[derive(serde::Deserialize)]
pub(super) struct OAuthCallbackQuery {
    /// Set on success. Present in 100% of successful redirects
    /// from both Google and Microsoft.
    code: Option<String>,
    /// Always present (in both success and error redirects) —
    /// echoed back verbatim from the consent URL. We use this to
    /// look up the matching pending session.
    state: Option<String>,
    /// Set when the operator clicked "Cancel" or the provider
    /// refused for any reason (e.g. consent screen
    /// misconfiguration). Surface to the UI as a session Error.
    error: Option<String>,
    /// Optional verbose description from the provider. Logged but
    /// NOT shown to the operator (often contains internal URLs).
    error_description: Option<String>,
}

/// Returns an HTML page the operator's popup tab renders after
/// consent. Always status 200 — the browser landed here via a
/// trusted redirect from the provider, and any error in our
/// processing is communicated through the page body + the
/// matching `oauth_status` poll.
pub(super) async fn oauth_callback(
    State(s): State<ApiState>,
    Path(provider_str): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(q): Query<OAuthCallbackQuery>,
) -> Response {
    // The callback runs without an admin bearer; the unguessable
    // `state` token is the proof-of-authorisation. Every branch
    // below must therefore validate the state before touching
    // anything else.
    let Some(state_token) = q.state else {
        return oauth_html_response(
            "Missing state",
            "OAuth callback missing required `state` parameter.",
            false,
        );
    };
    let Some(mut session) = s.oauth_sessions.get(&state_token) else {
        return oauth_html_response(
            "Unknown or expired session",
            "This OAuth session is unknown to the engine. It may have expired (10 min) or already been used. \
             Retry the Connect button in the Storage Admin tab.",
            false,
        );
    };

    let provider = match nexus_storage_cloud::Provider::from_kind(&provider_str) {
        Some(p) => p,
        None => {
            let msg = format!("unknown provider '{provider_str}'");
            s.oauth_sessions.set_status(
                &state_token,
                crate::oauth_sessions::SessionStatus::Error {
                    message: msg.clone(),
                },
            );
            return oauth_html_response("Unknown provider", &msg, false);
        }
    };

    // Defence-in-depth: the operator could in principle hand-edit
    // the redirect_uri before consent to land on the *other*
    // provider's callback route. The state token would still
    // resolve, but the kind would mismatch. Reject early before
    // we try to exchange a Drive code at a Microsoft token
    // endpoint (or vice versa).
    if session.provider != provider.as_str() {
        let msg = format!(
            "provider mismatch: session was started for '{}', callback hit '{}'",
            session.provider,
            provider.as_str()
        );
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Provider mismatch", &msg, false);
    }

    // Provider-side error (operator clicked Cancel, scope
    // mismatch, etc.). Mark the session Error and short-circuit
    // before any token-endpoint round-trip.
    if let Some(err_code) = q.error.as_deref() {
        let desc = q.error_description.as_deref().unwrap_or("");
        tracing::warn!(
            provider = provider.as_str(),
            handle = %session.handle,
            error_code = %err_code,
            error_desc = %desc,
            "OAuth callback: provider returned error"
        );
        let msg = format!("OAuth provider returned error: {err_code}");
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Consent declined or failed", &msg, false);
    }

    let Some(code) = q.code else {
        let msg =
            "OAuth callback missing both `code` and `error` — provider misbehaved".to_string();
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Malformed callback", &msg, false);
    };

    // Exchange the code for a refresh + access token pair.
    // `exchange_code` builds its own short-lived reqwest client
    // internally (20 s timeout) so the engine doesn't depend on
    // reqwest. Already maps every error to BackendError so we can
    // surface a clean status string without leaking the
    // provider's raw body.
    let tokens = match nexus_storage_cloud::exchange_code(
        provider,
        &code,
        &session.redirect_uri,
        &session.client_id,
        &session.client_secret,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("token exchange failed: {e}");
            tracing::warn!(
                provider = provider.as_str(),
                handle = %session.handle,
                error = %e,
                "OAuth callback: exchange_code failed"
            );
            s.oauth_sessions.set_status(
                &state_token,
                crate::oauth_sessions::SessionStatus::Error {
                    message: msg.clone(),
                },
            );
            return oauth_html_response("Token exchange failed", &msg, false);
        }
    };

    // Build the same JSON shape `put_storage_backend` accepts and
    // route through the existing encrypt + upsert pipeline.
    // Centralising on `put_storage_backend`'s helpers keeps the
    // crypto + audit + bus-publish surface in one place.
    let mut extra = serde_json::Map::new();
    if matches!(provider, nexus_storage_cloud::Provider::Gdrive) {
        if let Some(root) = session.root_folder_id.clone() {
            extra.insert(
                "root_folder_id".to_string(),
                serde_json::Value::String(root),
            );
        }
    }
    let mut config = serde_json::json!({
        "client_id": session.client_id,
        "client_secret": session.client_secret,
        "refresh_token": tokens.refresh_token,
        "account_email": session.account_email,
        "extra": serde_json::Value::Object(extra),
    });

    if let Err(e) = encrypt_cloud_refresh_token_in_place(&s, &mut config) {
        let msg = format!("refresh-token encryption failed: {}", e.1);
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Persist failed", &msg, false);
    }

    let config_json = config.to_string();
    let kind = provider.as_str();
    if let Err(e) = build_any_backend(
        &session.handle,
        kind,
        &config_json,
        s.admin_auth.admin_secret(),
    ) {
        let msg = format!("invalid backend config after exchange: {e}");
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Probe failed", &msg, false);
    }
    if let Err(e) = s
        .store
        .upsert_storage_backend(&session.handle, kind, &config_json)
        .await
    {
        let msg = format!("upsert failed: {e}");
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Persist failed", &msg, false);
    }
    if let Err(e) = rebuild_registry(&s).await {
        let msg = format!("registry rebuild failed: {}", e.1);
        s.oauth_sessions.set_status(
            &state_token,
            crate::oauth_sessions::SessionStatus::Error {
                message: msg.clone(),
            },
        );
        return oauth_html_response("Persist failed", &msg, false);
    }

    let audited = redacted_config_for_audit(&config);
    let resource_id = format!("{kind}/{}", session.handle);
    let after_str = serde_json::to_string(&serde_json::json!({
        "handle": session.handle,
        "kind": kind,
        "config": audited,
        "scope": tokens.scope,
    }))
    .ok();
    // No SessionContext: this endpoint runs outside the admin
    // gate (state token is the proof-of-authorisation). Helper
    // will record actor as `system:unknown`. Audit failures are
    // logged but do not fail the request.
    crate::auth::admin_audit::audit_admin_action(
        &s.store,
        None,
        &headers,
        peer.ip(),
        "oauth.callback",
        "admin/oauth",
        Some(resource_id.as_str()),
        nexus_store::audit::AuditOutcome::Success,
        None,
        after_str.as_deref(),
    )
    .await;
    let _ = s
        .bus
        .publish(
            topic::STORAGE_BACKENDS_CHANGED,
            &serde_json::json!({ "reason": "backend_oauth_completed", "handle": session.handle }),
        )
        .await;

    tracing::info!(
        provider = kind,
        handle = %session.handle,
        "OAuth auth-code flow: completed; backend persisted"
    );

    // Mark the session Complete so the UI's status poll picks it
    // up on the next tick.
    session.status = crate::oauth_sessions::SessionStatus::Complete {
        handle: session.handle.clone(),
    };
    s.oauth_sessions.set_status(
        &state_token,
        crate::oauth_sessions::SessionStatus::Complete {
            handle: session.handle.clone(),
        },
    );

    oauth_html_response(
        "Connected",
        &format!(
            "Backend `{}` has been connected. You can close this window — \
             the Storage Admin tab will refresh automatically.",
            session.handle
        ),
        true,
    )
}

/// Build the self-contained HTML page returned by `oauth_callback`.
/// Black-on-white, system font, no JS beyond a 2 s `window.close()`
/// on success. `success` controls the colour of the headline
/// dot — the polling UI is the authoritative status source, this
/// page is just operator-friendly chrome.
fn oauth_html_response(title: &str, body: &str, success: bool) -> Response {
    // Trivial sanitisation: the only operator-controlled segment
    // is `body`, and we already build it from constants or
    // `Display` impls. Run a tight HTML-escape anyway so any
    // future caller can't accidentally inject markup.
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    let dot = if success { "#1e9d4f" } else { "#c0392b" };
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Nexus OAuth — {title}</title>
<style>
  body {{ font: 14px/1.4 system-ui, -apple-system, sans-serif; max-width: 480px; margin: 4rem auto; padding: 1rem; color: #222; }}
  h1 {{ font-size: 1.2rem; display: flex; align-items: center; gap: 0.5rem; }}
  .dot {{ width: 10px; height: 10px; border-radius: 50%; background: {dot}; }}
  p.muted {{ color: #888; font-size: 12px; }}
</style>
</head>
<body>
<h1><span class="dot"></span>{title}</h1>
<p>{body}</p>
<p class="muted">This window will close automatically.</p>
<script>setTimeout(function() {{ try {{ window.close(); }} catch (e) {{}} }}, 2000);</script>
</body>
</html>"#,
        title = esc(title),
        body = esc(body),
        dot = dot,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

#[derive(serde::Deserialize)]
pub(super) struct OAuthStatusQuery {
    state: String,
}

/// Lifecycle status the polling UI sees. `pending` means the
/// callback hasn't run yet; `complete` carries the backend handle
/// the operator just minted; `error` carries a sanitised reason.
///
/// When the state isn't in the cache (either TTL'd out, never
/// existed, or was already cleared after a previous successful
/// poll) the handler returns **404 Not Found** rather than another
/// JSON variant — the UI's existing error-handling path already
/// treats a 404 here as "session expired before consent completed"
/// and surfaces a re-connect prompt. Keeping the wire format
/// strictly `pending | complete | error` mirrors the TS
/// discriminated union in `ui/src/api/types.ts::OAuthStatusResp`.
#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(super) enum OAuthStatusResp {
    Pending,
    Complete { handle: String },
    Error { message: String },
}

pub(super) async fn oauth_status(
    State(s): State<ApiState>,
    Query(q): Query<OAuthStatusQuery>,
) -> Result<Json<OAuthStatusResp>, ApiError> {
    let Some(session) = s.oauth_sessions.get(&q.state) else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "oauth session not found or expired".to_string(),
        ));
    };
    let resp = match session.status {
        crate::oauth_sessions::SessionStatus::Pending => OAuthStatusResp::Pending,
        crate::oauth_sessions::SessionStatus::Complete { ref handle } => {
            // Drop the session AFTER the UI has observed the
            // Complete state so a re-poll doesn't keep returning
            // the same row. Errors stay around for the full TTL
            // so the operator's UI can re-read the message if it
            // re-mounts the tab.
            let h = handle.clone();
            s.oauth_sessions.remove(&q.state);
            OAuthStatusResp::Complete { handle: h }
        }
        crate::oauth_sessions::SessionStatus::Error { ref message } => OAuthStatusResp::Error {
            message: message.clone(),
        },
    };
    Ok(Json(resp))
}
