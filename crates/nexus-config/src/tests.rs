use super::*;
use url::Url;

/// Minimal valid generic-email sink, mutated per assertion below.
fn email_sink_cfg() -> EmailSinkConfig {
    EmailSinkConfig {
        name: "site-ops".to_string(),
        smtp_host: "smtp.example.com".to_string(),
        smtp_port: 587,
        starttls: true,
        from_address: "nexus@example.com".to_string(),
        from_name: None,
        to: vec!["ops@example.com".to_string()],
        cc: Vec::new(),
        reply_to: None,
        subject_prefix: None,
        attach_snapshot: true,
        attach_clip: false,
        username: None,
        password: None,
        timeout_secs: 15,
    }
}

/// The `kind = "email"` discriminator is the `<kind>` half of every
/// `alert_sink_outbox.sink_id` — pin it so a serde rename can never
/// silently orphan historical outbox rows.
#[test]
fn email_sink_kind_tag_is_stable() {
    let cfg = SinkConfig::Email(email_sink_cfg());
    assert_eq!(cfg.kind(), "email");
    assert_eq!(cfg.name(), "site-ops");
    let json = serde_json::to_value(&cfg).unwrap();
    assert_eq!(json["kind"], "email");
}

/// #335: dwell / parked thresholds default to wall-clock durations,
/// and an explicit frame count — the fields operators already set —
/// still parses and wins.
#[test]
fn tracker_thresholds_default_to_durations_and_frames_still_parse() {
    let d = TrackerConfig::default();
    assert_eq!(d.static_object.dwell_frames, None);
    assert_eq!(d.annotator.parked_min_frames_to_flag, None);
    assert_eq!(d.bytetrack.max_lost_frames, 30);
    assert!(
        !d.static_object.anchor_persons,
        "person anchoring is opt-in"
    );

    let cfg: TrackerConfig =
        toml::from_str("[static_object]\ndwell_frames = 20\n[bytetrack]\nmax_lost_frames = 60\n")
            .unwrap();
    assert_eq!(cfg.static_object.dwell_frames, Some(20));
    assert_eq!(cfg.bytetrack.max_lost_frames, 60);
}

/// Minimal TOML (only the required fields) must parse, with every
/// optional field falling back to its documented default.
#[test]
fn email_sink_parses_from_minimal_toml() {
    let toml = r#"
kind = "email"
name = "site-ops"
smtp_host = "smtp.example.com"
from_address = "nexus@example.com"
to = ["ops@example.com"]
"#;
    let cfg: SinkConfig = toml::from_str(toml).unwrap();
    let SinkConfig::Email(e) = &cfg else {
        panic!("expected an email sink, got {cfg:?}");
    };
    assert_eq!(e.smtp_port, 587);
    assert!(e.starttls);
    assert!(e.attach_snapshot, "snapshots ride along by default");
    assert!(!e.attach_clip, "clips are opt-in (relay size limits)");
    assert_eq!(e.timeout_secs, 15);
    cfg.validate().unwrap();
}

/// A sink with no recipients would accept every outbox row and
/// deliver nothing — indistinguishable from working. Reject it.
#[test]
fn email_sink_rejects_empty_recipient_list() {
    let mut e = email_sink_cfg();
    e.to.clear();
    assert!(SinkConfig::Email(e).validate().is_err());
}

#[test]
fn email_sink_rejects_malformed_addresses() {
    for mutate in [
        (|e: &mut EmailSinkConfig| e.from_address = "nope".into()) as fn(&mut EmailSinkConfig),
        |e| e.to = vec!["nope".into()],
        |e| e.cc = vec!["nope".into()],
        |e| e.reply_to = Some("nope".into()),
    ] {
        let mut e = email_sink_cfg();
        mutate(&mut e);
        assert!(
            SinkConfig::Email(e.clone()).validate().is_err(),
            "expected rejection for {e:?}"
        );
    }
}

/// Half-configured SMTP AUTH silently authenticates as nobody, so
/// the pair must be all-or-nothing.
#[test]
fn email_sink_rejects_half_configured_auth() {
    let mut e = email_sink_cfg();
    e.username = Some("nexus@example.com".into());
    assert!(SinkConfig::Email(e).validate().is_err());
}

/// The relay password must never leave the appliance, and echoing
/// the sentinel back on a PUT must restore the stored value rather
/// than overwrite it with the sentinel.
#[test]
fn email_sink_password_round_trips_through_redaction() {
    let mut stored = email_sink_cfg();
    stored.username = Some("nexus@example.com".into());
    stored.password = Some("s3cret".into());
    let stored = SinkConfig::Email(stored);

    let mut leaving = stored.clone();
    leaving.redact_secrets();
    assert!(leaving.has_redacted_secret());
    assert!(
        !serde_json::to_string(&leaving).unwrap().contains("s3cret"),
        "live password escaped the box"
    );

    // The console echoes the sentinel back for an unchanged secret.
    let mut incoming = leaving;
    incoming.restore_redacted_secrets_from(&stored);
    assert!(!incoming.has_redacted_secret());
    let SinkConfig::Email(e) = incoming else {
        unreachable!()
    };
    assert_eq!(e.password.as_deref(), Some("s3cret"));
}

#[test]
fn defaults_validate() {
    let cfg = Config {
        cameras: vec![],
        ..Default::default()
    };
    cfg.validate().unwrap();
}

// --- M_NATIVE_ASPECT Phase 5: legacy shape remap ------------------------

#[test]
fn default_model_shape_is_on_the_ladder() {
    let m = ModelConfig::default();
    assert_eq!((m.input_width, m.input_height), (512, 288));
    assert_eq!(m.preset, "512x288");
    assert!(SHAPE_LADDER.contains(&(m.input_width, m.input_height)));
}

#[test]
fn remap_legacy_squares_to_ladder() {
    for (sq, want) in [
        ((640u32, 640u32), (512u32, 288u32)),
        ((960, 960), (1024, 576)),
        ((1280, 1280), (1536, 864)),
    ] {
        let mut m = ModelConfig {
            input_width: sq.0,
            input_height: sq.1,
            preset: sq.0.to_string(),
            ..Default::default()
        };
        m.remap_legacy_shapes();
        assert_eq!((m.input_width, m.input_height), want, "{sq:?}");
        assert_eq!(m.preset, format!("{}x{}", want.0, want.1));
    }
}

#[test]
fn remap_off_ladder_square_snaps_to_nearest_rung() {
    // 320² (102_400 px) is closest to the 512×288 rung (147_456 px).
    let mut m = ModelConfig {
        input_width: 320,
        input_height: 320,
        preset: "320".into(),
        ..Default::default()
    };
    m.remap_legacy_shapes();
    assert_eq!((m.input_width, m.input_height), (512, 288));
    assert_eq!(m.preset, "512x288");
}

#[test]
fn remap_is_idempotent_for_ladder_shapes() {
    let mut m = ModelConfig {
        input_width: 1536,
        input_height: 864,
        preset: "1536x864".into(),
        ..Default::default()
    };
    m.remap_legacy_shapes();
    assert_eq!((m.input_width, m.input_height), (1536, 864));
    assert_eq!(m.preset, "1536x864");
}

#[test]
fn remap_recurses_into_ensemble_members() {
    let mut parent = ModelConfig {
        kind: "ensemble".into(),
        members: vec![ModelConfig {
            input_width: 960,
            input_height: 960,
            preset: "960".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    parent.remap_legacy_shapes();
    assert_eq!(
        (
            parent.members[0].input_width,
            parent.members[0].input_height
        ),
        (1024, 576)
    );
    assert_eq!(parent.members[0].preset, "1024x576");
}

/// Phase 7.6.6 / REPO_BOUNDARY R5c §5 — the generic LAN device proxy
/// is opt-in: with `[lan_proxy]` absent (or defaulted) the feature is
/// off. This pins the default so the edge never proxies LAN traffic
/// unless an operator explicitly enables it.
#[test]
fn lan_proxy_is_off_by_default() {
    assert!(!LanProxyConfig::default().enabled);
    let cfg = Config {
        cameras: vec![],
        ..Default::default()
    };
    assert!(!cfg.lan_proxy.enabled);
}

/// Remote shell is opt-in on the appliance itself, independent of
/// anything the org enables in the cloud console. Both switches must
/// be on for a session to be possible, and this pins the edge half.
#[test]
fn remote_access_is_off_by_default() {
    assert!(!RemoteAccessConfig::default().enabled);
    let cfg = Config {
        cameras: vec![],
        ..Default::default()
    };
    assert!(!cfg.remote_access.enabled);
    assert_eq!(cfg.remote_access.target, "127.0.0.1:22");
}

/// The cloud may switch remote access on unless the box owner opts out,
/// and a `[remote_access]` block written before the key existed must
/// read as "may" — that is what lets an OTA'd core gain the cloud toggle
/// without a `nexus.toml` rewrite. `enabled` itself stays off.
#[test]
fn remote_access_cloud_may_enable_defaults_true_and_parses_an_opt_out() {
    assert!(RemoteAccessConfig::default().cloud_may_enable);
    assert!(
        Config {
            cameras: vec![],
            ..Default::default()
        }
        .remote_access
        .cloud_may_enable
    );

    let legacy: RemoteAccessConfig =
        toml::from_str("enabled = false\ntarget = \"127.0.0.1:22\"\n").unwrap();
    assert!(legacy.cloud_may_enable, "a missing key means the cloud may");
    assert!(!legacy.enabled);

    let opted_out: RemoteAccessConfig = toml::from_str("cloud_may_enable = false\n").unwrap();
    assert!(!opted_out.cloud_may_enable);
    assert!(!opted_out.enabled);
}

/// The shell target is a *local* address by construction — the point
/// of the feature is reaching this box's own sshd, not turning the
/// appliance into a jump host for the rest of the customer's LAN.
#[test]
fn remote_access_target_must_be_local() {
    for good in [
        "127.0.0.1:22",
        "localhost:22",
        "192.168.1.10:22",
        "10.0.0.4:2222",
        "172.16.5.5:22",
        "169.254.1.1:22",
        "[::1]:22",
    ] {
        let c = RemoteAccessConfig {
            enabled: true,
            target: good.to_string(),
            ..Default::default()
        };
        assert!(c.target_is_local(), "{good} should be local");
    }
    for bad in ["8.8.8.8:22", "example.com:22", "203.0.113.7:2222"] {
        let c = RemoteAccessConfig {
            enabled: true,
            target: bad.to_string(),
            ..Default::default()
        };
        assert!(!c.target_is_local(), "{bad} should not be local");
    }
}

/// A non-local target is a hard config error, not a warning: better to
/// refuse to start than to quietly ship an SSRF pivot.
#[test]
fn validate_refuses_a_public_shell_target() {
    let cfg = Config {
        cameras: vec![],
        remote_access: RemoteAccessConfig {
            enabled: true,
            target: "8.8.8.8:22".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

/// M-Alert-Clip — alert clips are ON by default; operators disable
/// them per-org / per-core from the cloud console's delivery settings
/// (`DeliverySettings.attach_alert_clip`), not by editing config.
/// Pins the defaults so a regression can't silently change the
/// capability switch or the window/timeout tunables.
#[test]
fn alert_clips_are_on_by_default() {
    let d = AlertClipsConfig::default();
    assert!(d.enabled);
    assert_eq!(d.pre_secs, 3);
    assert_eq!(d.post_secs, 5);
    assert_eq!(d.max_encode_width, 1280);
    assert_eq!(d.build_timeout_secs, 30);
    // The ClipsConfig default embeds the enabled AlertClipsConfig.
    assert!(ClipsConfig::default().alert_clips.enabled);
    // A `[clips]` section without `[clips.alert_clips]` deserialises to
    // the enabled default.
    let clips: ClipsConfig = toml::from_str("").unwrap();
    assert!(clips.alert_clips.enabled);
    assert_eq!(clips.alert_clips.post_secs, 5);
    // An explicit `enabled = false` still hard-disables the capability.
    let off: ClipsConfig = toml::from_str("[alert_clips]\nenabled = false\n").unwrap();
    assert!(!off.alert_clips.enabled);
}

/// Every alert/event must get an underlying full-resolution motion
/// clip by default, so a keyframe-only detection (no sustained
/// motion → no tracker `Born`) still leaves surrounding native-res
/// video. Pins the default ON and the explicit override OFF.
#[test]
fn record_motion_clip_on_alert_is_on_by_default() {
    assert!(ClipsConfig::default().record_motion_clip_on_alert);
    // Missing key deserialises to the enabled default.
    let clips: ClipsConfig = toml::from_str("").unwrap();
    assert!(clips.record_motion_clip_on_alert);
    // Explicit `false` disables it (reverts to unlinked alerts on
    // motionless frames).
    let off: ClipsConfig = toml::from_str("record_motion_clip_on_alert = false\n").unwrap();
    assert!(!off.record_motion_clip_on_alert);
}

#[test]
fn sureview_secret_redact_round_trip() {
    let json =
        r#"{"kind":"sureview","name":"front","api_key":"LIVE-KEY","system_identifier":"site-1"}"#;
    let stored: SinkConfig = serde_json::from_str(json).unwrap();

    // Redaction replaces the live key with the sentinel.
    let mut redacted = stored.clone();
    redacted.redact_secrets();
    assert!(redacted.has_redacted_secret());
    if let SinkConfig::SureView(s) = &redacted {
        assert_eq!(s.api_key, SinkConfig::REDACTED_SECRET);
    } else {
        panic!("expected sureview");
    }

    // An incoming PUT that echoes the sentinel back gets the
    // live key restored from the stored config.
    let mut incoming = redacted.clone();
    incoming.restore_redacted_secrets_from(&stored);
    assert!(!incoming.has_redacted_secret());
    if let SinkConfig::SureView(s) = &incoming {
        assert_eq!(s.api_key, "LIVE-KEY");
    } else {
        panic!("expected sureview");
    }
}

#[test]
fn restore_is_a_noop_when_secret_changed() {
    let stored: SinkConfig = serde_json::from_str(
        r#"{"kind":"sureview","name":"front","api_key":"OLD","system_identifier":"s"}"#,
    )
    .unwrap();
    // Operator typed a brand-new key — NOT the sentinel — so
    // restore must leave it untouched.
    let mut incoming: SinkConfig = serde_json::from_str(
        r#"{"kind":"sureview","name":"front","api_key":"NEW","system_identifier":"s"}"#,
    )
    .unwrap();
    incoming.restore_redacted_secrets_from(&stored);
    if let SinkConfig::SureView(s) = &incoming {
        assert_eq!(s.api_key, "NEW");
    } else {
        panic!("expected sureview");
    }
}

#[test]
fn pool_requires_workers() {
    let mut cfg = Config::default();
    cfg.inference.backend = InferenceBackendKind::Pool;
    cfg.inference.workers = 0;
    assert!(cfg.validate().is_err());
}

// M-Admin Phase 0 closeout — secure-by-default. The legacy
// `None` and `DevToken` variants are gone; a fresh install
// (empty TOML, or one with `[auth]\n` only) lands on `Local`
// and the engine auto-provisions an admin secret + prints a
// one-time admin OTP at WARN. Failing this test would mean a
// new install could silently boot without a real credential.
#[test]
fn auth_mode_default_is_local() {
    let auth: AuthConfig = Default::default();
    assert_eq!(auth.mode, AuthMode::Local);
    let parsed: AuthConfig = toml::from_str("").unwrap();
    assert_eq!(parsed.mode, AuthMode::Local);
}

// M-Admin Phase 0 closeout — pre-existing dev installs whose
// nexus.toml has no `[auth]` block now land on `Local` (the
// new default). No grandfathering; the engine auto-provisions
// an admin secret on first boot.
#[test]
fn load_with_compat_missing_auth_lands_on_local() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nexus.toml");
    std::fs::write(&path, "[server]\napi_bind = \"127.0.0.1:8089\"\n").unwrap();
    let (cfg, _notice) = Config::load_with_compat(&path).unwrap();
    assert_eq!(cfg.auth.mode, AuthMode::Local);
}

/// Legacy `auth.mode = "none"` from pre-Phase-0 configs MUST
/// be rejected at load time with a clear error so operators
/// upgrade explicitly rather than silently landing on a
/// different posture.
#[test]
fn load_with_compat_rejects_legacy_mode_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nexus.toml");
    std::fs::write(&path, "[auth]\nmode = \"none\"\n").unwrap();
    let err = Config::load_with_compat(&path).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("none") && msg.contains("no longer supported"),
        "{msg}"
    );
}

/// Same as above for the retired `dev_token` mode.
#[test]
fn load_with_compat_rejects_legacy_mode_dev_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nexus.toml");
    std::fs::write(&path, "[auth]\nmode = \"dev_token\"\n").unwrap();
    let err = Config::load_with_compat(&path).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("dev_token") && msg.contains("no longer supported"),
        "{msg}"
    );
}

/// The legacy-mode reject must ignore the same string when it
/// only appears inside a `#` comment (e.g. nexus.example.toml
/// listing the historical option set in a doc comment).
#[test]
fn load_with_compat_ignores_legacy_mode_in_comment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nexus.toml");
    std::fs::write(
        &path,
        "[auth]\nmode = \"local\"  # historical: none | dev_token | local\n",
    )
    .unwrap();
    let (cfg, _) = Config::load_with_compat(&path).unwrap();
    assert_eq!(cfg.auth.mode, AuthMode::Local);
}

// -----------------------------------------------------------------------
// M6 — AuthMode + OidcConfig + RuntimeAuthConfig
// -----------------------------------------------------------------------

#[test]
fn auth_mode_local_and_hybrid_parse() {
    for (s, expected) in [
        ("local", AuthMode::Local),
        ("oidc", AuthMode::Oidc),
        ("hybrid", AuthMode::Hybrid),
    ] {
        let toml_src = format!("mode = \"{s}\"\n");
        let parsed: AuthConfig = toml::from_str(&toml_src).unwrap();
        assert_eq!(parsed.mode, expected, "round-trip for {s:?}");
    }
}

#[test]
fn auth_mode_allows_local_and_oidc_matrix() {
    // Pinned matrix so future variants don't accidentally flip
    // a bit and let an `Oidc`-only deployment accept local login.
    let cases: &[(AuthMode, bool, bool)] = &[
        (AuthMode::Local, true, false),
        (AuthMode::Oidc, false, true),
        (AuthMode::Hybrid, true, true),
    ];
    for (mode, local, oidc) in cases.iter().copied() {
        assert_eq!(mode.allows_local(), local, "{mode:?}.allows_local");
        assert_eq!(mode.allows_oidc(), oidc, "{mode:?}.allows_oidc");
    }
}

#[test]
fn oidc_config_defaults_supply_sane_scopes_and_claims() {
    let src = r#"
issuer = "https://auth.example.com"
audience = "nexus"
"#;
    let cfg: OidcConfig = toml::from_str(src).unwrap();
    assert_eq!(cfg.scopes, vec!["openid", "profile", "email", "groups"]);
    assert_eq!(
        cfg.role_claims,
        vec!["groups", "roles", "https://nexus.local/role"]
    );
    assert!(cfg.role_map.is_empty());
    assert!(!cfg.deny_unmapped);
    assert!(cfg.client_id.is_none());
    assert!(cfg.display_name.is_none());
}

#[test]
fn oidc_role_map_parses_per_role_lists() {
    let src = r#"
issuer = "https://auth.example.com"
audience = "nexus"
deny_unmapped = true

[role_map]
admin = ["nexus-admins"]
operator = ["nexus-operators", "security-team"]
"#;
    let cfg: OidcConfig = toml::from_str(src).unwrap();
    assert!(cfg.deny_unmapped);
    assert_eq!(cfg.role_map.admin, vec!["nexus-admins"]);
    assert_eq!(
        cfg.role_map.operator,
        vec!["nexus-operators", "security-team"]
    );
    assert!(cfg.role_map.viewer.is_empty());
    assert!(!cfg.role_map.is_empty());
}

#[test]
fn runtime_auth_lockout_defaults_match_design() {
    // The defaults are wire-pinned (5 / 15min / 15min) — these
    // are the OWASP-ish baseline the M6 design committed to. If
    // a future PR wants to tune them, change this test in lock-
    // step with the doc.
    let r: RuntimeAuthConfig = Default::default();
    assert_eq!(r.lockout.max_attempts, 5);
    assert_eq!(r.lockout.window_secs, 900);
    assert_eq!(r.lockout.lockout_secs, 900);
}

#[test]
fn runtime_audit_retention_default_is_one_year() {
    let r: RuntimeAuditConfig = Default::default();
    assert_eq!(r.retention_days, 365);
}

#[test]
fn runtime_auth_overrides_round_trip_via_toml() {
    let src = r#"
state_dir = "/var/lib/nexus/state"

[auth.lockout]
max_attempts = 10
window_secs = 300
lockout_secs = 60

[audit]
retention_days = 90
"#;
    let rc: RuntimeConfig = toml::from_str(src).unwrap();
    assert_eq!(rc.auth.lockout.max_attempts, 10);
    assert_eq!(rc.auth.lockout.window_secs, 300);
    assert_eq!(rc.auth.lockout.lockout_secs, 60);
    assert_eq!(rc.audit.retention_days, 90);
}

// -----------------------------------------------------------------
// [runtime.decode] hardware-decode knob
// -----------------------------------------------------------------

/// Upgrade-reach guarantee (Wedge P3.1): a config that predates
/// the `[runtime.decode]` knob (no `[decode]` table at all)
/// deserialises with `DecodeMode::Auto`, so an old core that
/// upgrades onto a VA-capable box auto-enables hardware decode
/// with zero config migration.
#[test]
fn runtime_decode_defaults_to_auto() {
    let from_default: RuntimeConfig = Default::default();
    assert_eq!(from_default.decode.mode, DecodeMode::Auto);
    let from_empty: RuntimeConfig = toml::from_str("").unwrap();
    assert_eq!(from_empty.decode.mode, DecodeMode::Auto);
    // A legacy config that sets an unrelated [runtime] knob but
    // carries no [decode] table still defaults to Auto.
    let legacy: RuntimeConfig = toml::from_str("state_dir = \"/var/lib/nexus/state\"").unwrap();
    assert_eq!(legacy.decode.mode, DecodeMode::Auto);
}

/// Upgrade-reach guarantee at the TOP-LEVEL `Config` boundary (Wedge
/// P3.1): a complete operator `nexus.toml` that predates the decode
/// knob — i.e. has `[runtime]`, `[server]`, `[inference]` … but no
/// `[runtime.decode]` section — still yields `DecodeMode::Auto`. This
/// is the exact shape a preserved pre-upgrade config has, and Auto is
/// what lets the upgraded engine auto-probe for VA decode without any
/// config migration. If this regresses (e.g. the default flips to
/// `Software`), every upgraded box silently loses hardware decode.
#[test]
fn full_config_without_decode_section_defaults_to_auto() {
    // A trimmed-but-realistic legacy config: several sections present,
    // deliberately NO [runtime.decode].
    let legacy = r#"
[runtime]
worker_threads = 16
blocking_threads = 16

[server]
api_bind = "0.0.0.0:8089"

[inference]
backend = "pool"
ep_priority = ["hailo", "cpu"]

[bus]
capacity = 2048
"#;
    let cfg: Config = toml::from_str(legacy).expect("legacy config must parse");
    assert_eq!(cfg.runtime.decode.mode, DecodeMode::Auto);
    // Sanity: the sections that ARE present still parsed.
    assert_eq!(cfg.runtime.worker_threads, 16);
    assert_eq!(cfg.inference.ep_priority, vec!["hailo", "cpu"]);
}

/// `[decode] mode = "..."` round-trips for every variant, and the
/// serialised token matches the documented lowercase spelling
/// (the engine and installer both rely on these exact tokens).
#[test]
fn runtime_decode_mode_round_trips_via_toml() {
    for (token, mode) in [
        ("auto", DecodeMode::Auto),
        ("va", DecodeMode::Va),
        ("msdk", DecodeMode::Msdk),
        ("nvdec", DecodeMode::Nvdec),
        ("software", DecodeMode::Software),
    ] {
        let src = format!("[decode]\nmode = \"{token}\"\n");
        let rc: RuntimeConfig = toml::from_str(&src).unwrap();
        assert_eq!(rc.decode.mode, mode, "parse {token}");
        let out = toml::to_string(&RuntimeDecodeConfig { mode }).unwrap();
        assert!(
            out.contains(&format!("mode = \"{token}\"")),
            "serialise {token}: {out}"
        );
    }
}

/// `[runtime.decode]` rejects unknown keys (deny_unknown_fields)
/// so a typo'd knob fails loudly at boot instead of being
/// silently ignored.
#[test]
fn runtime_decode_rejects_unknown_key() {
    let src = "[decode]\nmode = \"auto\"\nbogus = true\n";
    let err = toml::from_str::<RuntimeConfig>(src).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("bogus") || msg.contains("unknown"),
        "expected unknown-field error, got: {msg}"
    );
}

// -----------------------------------------------------------------
// M3.1 — VisualPromptRef wire shape + defaults
// -----------------------------------------------------------------

/// `runtime.visual_prompts_dir` defaults to /var/lib/nexus/visual_prompts
/// when the TOML omits it. Asserts the default helper agrees with
/// the spec in docs/M3_OPEN_VOCAB_VISUAL.md so a future tweak of
/// either side trips this lock.
#[test]
fn runtime_visual_prompts_dir_default_matches_spec() {
    let r: RuntimeConfig = Default::default();
    assert_eq!(
        r.visual_prompts_dir,
        std::path::PathBuf::from("/var/lib/nexus/visual_prompts")
    );
    let from_empty: RuntimeConfig = toml::from_str("").unwrap();
    assert_eq!(
        from_empty.visual_prompts_dir,
        std::path::PathBuf::from("/var/lib/nexus/visual_prompts")
    );
}

/// Operators can override `runtime.visual_prompts_dir` in TOML
/// (operator may want it on a faster SSD partition separate from
/// `state_dir`). Confirms serde sees the field.
#[test]
fn runtime_visual_prompts_dir_round_trips_via_toml() {
    let src = r#"
state_dir = "/var/lib/nexus/state"
visual_prompts_dir = "/mnt/fast/visual_prompts"
"#;
    let rc: RuntimeConfig = toml::from_str(src).unwrap();
    assert_eq!(
        rc.visual_prompts_dir,
        std::path::PathBuf::from("/mnt/fast/visual_prompts")
    );
}

/// Wire-shape lock: a `[[cameras]]` table with `visual_prompts =
/// [{ id = 1, label = "amazon_van" }]` round-trips through TOML
/// → CameraConfig → JSON. Catches accidental rename / removal of
/// the field or its sub-keys.
#[test]
fn camera_visual_prompts_round_trip_via_toml() {
    let src = r#"
id = 1
name = "front_door"
url = "rtsp://example/cam"
visual_prompts = [
  { id = 1, label = "amazon_van" },
  { id = 7, label = "fedex_truck" },
]
"#;
    let cam: CameraConfig = toml::from_str(src).unwrap();
    assert_eq!(cam.detector.visual_prompts.len(), 2);
    assert_eq!(cam.detector.visual_prompts[0].id, 1);
    assert_eq!(cam.detector.visual_prompts[0].label, "amazon_van");
    assert_eq!(cam.detector.visual_prompts[1].id, 7);
    assert_eq!(cam.detector.visual_prompts[1].label, "fedex_truck");

    // The field must remain flat at the wire boundary (no
    // `[detector]` envelope leaked by the existing #[serde(flatten)]
    // refactor).
    let v = serde_json::to_value(&cam).unwrap();
    let obj = v.as_object().unwrap();
    assert!(obj.contains_key("visual_prompts"));
    assert!(!obj.contains_key("detector"));
}

/// A camera that omits `visual_prompts` must still load (existing
/// nexus.toml files predate M3.1). Defaults to an empty Vec via
/// `#[serde(default)]` on the field.
#[test]
fn camera_visual_prompts_defaults_to_empty_when_absent() {
    let src = r#"
id = 1
name = "front_door"
url = "rtsp://example/cam"
"#;
    let cam: CameraConfig = toml::from_str(src).unwrap();
    assert!(cam.detector.visual_prompts.is_empty());
}

/// SPEC-069 Phase 2 — `analysis_url` is additive. Every config
/// written before it existed must still load, and a camera that
/// never uses it must serialise byte-identically to how it did
/// before, or an OTA silently rewrites every appliance's config.
#[test]
fn analysis_url_is_absent_from_a_camera_that_does_not_use_it() {
    let src = r#"
id = 1
name = "front_door"
url = "rtsp://example/cam"
"#;
    let cam: CameraConfig = toml::from_str(src).unwrap();
    assert!(cam.ingest.analysis_url.is_none());

    let obj = serde_json::to_value(&cam).unwrap();
    assert!(
        !obj.as_object().unwrap().contains_key("analysis_url"),
        "an unset analysis_url must not appear on the wire"
    );
}

/// And when it *is* set it survives the round trip flat, beside
/// `url` — the admin API and the cloud both address it by that name.
#[test]
fn analysis_url_round_trips_flat_beside_url() {
    let src = r#"
id = 1
name = "front_door"
url = "rtsp://example/Streaming/Channels/101"
analysis_url = "rtsp://example/Streaming/Channels/102"
"#;
    let cam: CameraConfig = toml::from_str(src).unwrap();
    assert_eq!(
        cam.ingest.analysis_url.as_ref().map(Url::as_str),
        Some("rtsp://example/Streaming/Channels/102")
    );

    let v = serde_json::to_value(&cam).unwrap();
    let obj = v.as_object().unwrap();
    assert!(obj.contains_key("analysis_url"));
    assert!(!obj.contains_key("ingest"));
}

/// Phase 7.6.1 — ONVIF endpoint + credentials round-trip through the
/// `cameras.config_json` blob (which is just `serde_json` of
/// `CameraConfig`), and a camera that omits them serializes WITHOUT
/// an `onvif` key so existing blobs stay byte-identical (no SQLite
/// migration). Credentials are edge-resident only; the roster
/// redaction is asserted separately in `nexus-engine`.
#[test]
fn camera_onvif_round_trips_and_is_omitted_when_empty() {
    // Absent → empty, and serializes away (config_json unchanged).
    let bare: CameraConfig = toml::from_str(
        r#"
id = 1
name = "front_door"
url = "rtsp://example/cam"
"#,
    )
    .unwrap();
    assert!(bare.onvif.is_empty());
    let v = serde_json::to_value(&bare).unwrap();
    assert!(
        !v.as_object().unwrap().contains_key("onvif"),
        "empty onvif must be skipped so config_json is unchanged"
    );

    // Present → parses, and JSON round-trips losslessly.
    let cam: CameraConfig = toml::from_str(
            r#"
id = 2
name = "ptz_cam"
url = "rtsp://example/ptz"
onvif = { endpoint = "http://192.168.1.64/onvif/device_service", username = "admin", password = "s3cret" }
"#,
        )
        .unwrap();
    assert_eq!(
        cam.onvif.endpoint.as_deref(),
        Some("http://192.168.1.64/onvif/device_service")
    );
    assert_eq!(cam.onvif.username.as_deref(), Some("admin"));
    assert_eq!(cam.onvif.password.as_deref(), Some("s3cret"));

    let json = serde_json::to_string(&cam).unwrap();
    let back: CameraConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.onvif, cam.onvif);
}

/// Phase 7.6.7 — the `talk_down` (speaker) block round-trips through
/// JSON and serializes away when empty so non-speaker cameras keep
/// an unchanged `config_json` (no SQLite migration). Edge-resident
/// only; the roster redaction is asserted in `nexus-engine`.
#[test]
fn camera_talk_down_round_trips_and_is_omitted_when_empty() {
    // Absent → empty, serializes away.
    let bare: CameraConfig = toml::from_str(
        r#"
id = 1
name = "fixed_cam"
url = "rtsp://example/cam"
"#,
    )
    .unwrap();
    assert!(bare.talk_down.is_empty());
    let v = serde_json::to_value(&bare).unwrap();
    assert!(
        !v.as_object().unwrap().contains_key("talk_down"),
        "empty talk_down must be skipped so config_json is unchanged"
    );

    // Present → parses, and JSON round-trips losslessly.
    let cam: CameraConfig = toml::from_str(
            r#"
id = 2
name = "speaker_cam"
url = "rtsp://example/spk"
talk_down = { speaker_present = true, backchannel_codec = "PCMU", backchannel_url = "rtsp://example/spk/backchannel" }
"#,
        )
        .unwrap();
    assert!(cam.talk_down.speaker_present);
    assert_eq!(cam.talk_down.backchannel_codec.as_deref(), Some("PCMU"));
    assert_eq!(
        cam.talk_down.backchannel_url.as_deref(),
        Some("rtsp://example/spk/backchannel")
    );

    let json = serde_json::to_string(&cam).unwrap();
    let back: CameraConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.talk_down, cam.talk_down);
}

/// `CameraConfigUpdate` (fan-pushed to every detector slot on
/// reload) carries the visual-prompt attachments. JSON round-trip
/// asserts the new field is on the wire and defaults to empty
/// when an older publisher omits it.
#[test]
fn camera_config_update_visual_prompts_round_trip_via_json() {
    let update = CameraConfigUpdate {
        camera_id: 42,
        prompts: vec!["person".into()],
        visual_prompts: vec![VisualPromptRef {
            id: 9,
            label: "delivery_van".into(),
        }],
        model: ModelConfig::default(),
        generation: 3,
    };
    let json = serde_json::to_string(&update).unwrap();
    let back: CameraConfigUpdate = serde_json::from_str(&json).unwrap();
    assert_eq!(back.visual_prompts.len(), 1);
    assert_eq!(back.visual_prompts[0].id, 9);
    assert_eq!(back.visual_prompts[0].label, "delivery_van");

    // Backwards-compat: a publisher that predates the field
    // emits JSON without `visual_prompts`. Receiver must accept it.
    let legacy = r#"{
            "camera_id": 42,
            "prompts": ["person"],
            "model": {},
            "generation": 3
        }"#;
    let parsed: CameraConfigUpdate = serde_json::from_str(legacy).unwrap();
    assert!(parsed.visual_prompts.is_empty());
}

/// `VisualPromptRef` denies unknown fields — typos in admin JSON
/// surface as an error rather than silently dropping (e.g.
/// `lable` instead of `label`).
#[test]
fn visual_prompt_ref_denies_unknown_fields() {
    let bad = r#"{ "id": 1, "label": "amazon_van", "lable": "typo" }"#;
    assert!(serde_json::from_str::<VisualPromptRef>(bad).is_err());
}

/// Wire-shape lock for the camera/rule refactor: the public TOML
/// keys for every shipped config under `config/` must still parse
/// after the `#[serde(flatten)]` regrouping (no nested `[ingest]`,
/// `[detector]`, `[gates]`, etc. tables introduced). Every camera
/// keeps reading `url`, `enabled`, `max_fps`, `prompts`,
/// `model_override`, `parking_lot_mode` at the top of the
/// `[[cameras]]` array; every rule keeps reading `when`,
/// `severity`, `camera_filter`, `zones`, `min_track_age_ms`,
/// `consecutive_frames`, `cooldown_ms` at the top of `[[rules]]`.
/// If this test ever needs a fixture update, you have broken
/// every existing operator's nexus.toml — back out the change.
#[test]
fn shipped_configs_round_trip_flat_wire_shape() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root above crates/nexus-config");
    let config_dir = repo_root.join("config");
    // Scan the shipped top-level `config/` samples. The per-box
    // templates that used to live under `config/tiers/` were retired
    // with the capability-based generator (M_HWCONFIG): the live
    // /etc/nexus/nexus.toml is now produced by `nexus-probe
    // emit-config`, whose output is type-checked + `deny_unknown_fields`
    // safe by construction (built from a real `Config`), and the
    // generated shape is covered by the nexus-probe golden_config
    // fixtures. This test still guards the hand-written sample configs.
    let mut toml_paths: Vec<std::path::PathBuf> = Vec::new();
    let entries = std::fs::read_dir(&config_dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", config_dir.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().and_then(|s| s.to_str()) == Some("toml") {
            toml_paths.push(path);
        }
    }
    let mut checked = 0usize;
    for path in &toml_paths {
        let cfg = Config::load(path).unwrap_or_else(|e| panic!("load {}: {e}", path.display()));
        // Validate via the same path the engine uses on boot.
        cfg.validate()
            .unwrap_or_else(|e| panic!("validate {}: {e}", path.display()));
        for cam in &cfg.cameras {
            let v = serde_json::to_value(cam).unwrap();
            let obj = v.as_object().expect("CameraConfig serializes as an object");
            // Top-level keys must be flat — no `ingest`/`detector`/`behavior` envelopes.
            for forbidden in ["ingest", "detector", "behavior"] {
                assert!(
                    !obj.contains_key(forbidden),
                    "{}: CameraConfig leaked a `{forbidden}` envelope to the wire \
                         (broke #[serde(flatten)] guarantee)",
                    path.display()
                );
            }
            // Anchor a few must-stay-flat keys so an accidental
            // un-flatten in the future tips this test over loudly.
            for required in ["id", "name", "url", "enabled"] {
                assert!(
                    obj.contains_key(required),
                    "{}: CameraConfig dropped flat key `{required}`",
                    path.display()
                );
            }
        }
        for rule in &cfg.rules.inline {
            let v = serde_json::to_value(rule).unwrap();
            let obj = v.as_object().expect("RuleConfig serializes as an object");
            for forbidden in ["predicate", "gates", "debounce"] {
                assert!(
                    !obj.contains_key(forbidden),
                    "{}: RuleConfig leaked a `{forbidden}` envelope to the wire \
                         (broke #[serde(flatten)] guarantee)",
                    path.display()
                );
            }
            for required in ["id", "name", "when", "severity"] {
                assert!(
                    obj.contains_key(required),
                    "{}: RuleConfig dropped flat key `{required}`",
                    path.display()
                );
            }
        }
        checked += 1;
    }
    assert!(
        checked >= 3,
        "expected to round-trip at least 3 shipped top-level sample TOMLs \
             (nexus.example.toml, single-camera.toml, single-camera.youtube.toml) \
             from {} (found {checked})",
        config_dir.display()
    );
}

// -----------------------------------------------------------------------
// M_TILE_REINFER (G1) Phase B1 — per-camera tile knobs.
// -----------------------------------------------------------------------

/// Default `CameraBehavior` leaves every tile knob unset, so
/// pre-G1 configs round-trip unchanged.
#[test]
fn tile_knobs_default_to_none() {
    let b = CameraBehavior::default();
    assert!(b.tile_enabled.is_none());
    assert!(b.tile_trigger.is_none());
    assert!(b.tile_max_per_frame.is_none());
    assert!(b.tile_grid.is_none());
}

/// Wire-shape lock — flat top-level TOML keys, snake-case grid
/// variant. The 4 fields land directly under `[[cameras]]` thanks
/// to the existing `#[serde(flatten)] behavior` on `CameraConfig`.
#[test]
fn tile_knobs_round_trip_via_toml() {
    let src = r#"
id = 1
name = "front_door"
url = "rtsp://example/cam"
tile_enabled = true
tile_trigger = 12
tile_max_per_frame = 4
tile_grid = "g3x3"
"#;
    let cam: CameraConfig = toml::from_str(src).unwrap();
    assert_eq!(cam.behavior.tile_enabled, Some(true));
    assert_eq!(cam.behavior.tile_trigger, Some(12));
    assert_eq!(cam.behavior.tile_max_per_frame, Some(4));
    assert_eq!(cam.behavior.tile_grid, Some(TileGridConfig::G3x3));

    // Wire JSON must keep the keys flat (no `behavior` envelope).
    let v = serde_json::to_value(&cam).unwrap();
    let obj = v.as_object().unwrap();
    assert!(obj.contains_key("tile_enabled"));
    assert!(obj.contains_key("tile_grid"));
    assert!(!obj.contains_key("behavior"));
}

/// `tile_enabled = true` with the global model `kind = "ensemble"`
/// must be rejected at load time. The ban is per the wedge plan
/// (`docs/edge-core/M_TILE_REINFER.md`): per-member tile budgeting
/// is out of scope for v1.
#[test]
fn tile_enabled_rejected_on_ensemble_global() {
    let mut cfg = Config::default();
    cfg.inference.model.kind = "ensemble".into();
    let cam_src = r#"
id = 1
name = "c1"
url = "rtsp://example/cam"
tile_enabled = true
"#;
    cfg.cameras.push(toml::from_str(cam_src).unwrap());
    let err = cfg.validate().unwrap_err();
    assert!(
        err.to_string().contains("ensemble") && err.to_string().contains("tile_enabled"),
        "{err}"
    );
}

/// Same ban, triggered via per-camera `model_override`. The
/// effective-kind resolution prefers the override over the global.
#[test]
fn tile_enabled_rejected_on_ensemble_override() {
    let mut cfg = Config::default();
    // Global is fine (default kind), but the camera overrides
    // to ensemble.
    let cam_src = r#"
id = 1
name = "c1"
url = "rtsp://example/cam"
tile_enabled = true

[model_override]
kind = "ensemble"
"#;
    cfg.cameras.push(toml::from_str(cam_src).unwrap());
    let err = cfg.validate().unwrap_err();
    assert!(err.to_string().contains("ensemble"), "{err}");
}

/// `tile_enabled = true` is fine when the camera overrides
/// AWAY from a global ensemble back to a single detector — the
/// override wins.
#[test]
fn tile_enabled_ok_when_override_escapes_global_ensemble() {
    let mut cfg = Config::default();
    cfg.inference.model.kind = "ensemble".into();
    let cam_src = r#"
id = 1
name = "c1"
url = "rtsp://example/cam"
tile_enabled = true

[model_override]
kind = "yolo_world"
"#;
    cfg.cameras.push(toml::from_str(cam_src).unwrap());
    cfg.validate()
        .expect("override to non-ensemble must validate");
}

/// `tile_enabled = None` or `Some(false)` is always accepted,
/// even on ensemble — the ban only fires for true.
#[test]
fn tile_disabled_is_always_accepted_even_on_ensemble() {
    let mut cfg = Config::default();
    cfg.inference.model.kind = "ensemble".into();
    let cam_src = r#"
id = 1
name = "c1"
url = "rtsp://example/cam"
tile_enabled = false
tile_trigger = 12
"#;
    cfg.cameras.push(toml::from_str(cam_src).unwrap());
    cfg.validate().unwrap();
}

/// The config file seeds the store's camera rows, and a camera number
/// JSON has no literal for (TOML's `inf` and `nan`, or `1e39`, which is
/// beyond f32's range and parses to infinity) is stored as `null`, which
/// no later read of the camera's row can decode. So load refuses one,
/// wherever a camera carries a float.
#[test]
fn a_camera_number_json_cannot_hold_is_refused_at_load() {
    let zone = |vertex: &str| {
        format!(
                "[[zones]]\nid = \"z1\"\nname = \"door\"\npolygon = [{vertex}, [0.1, 0.1], [0.2, 0.9]]\n"
            )
    };
    for (field, cam_src) in [
        ("zone x", zone("[inf, 0.5]")),
        ("zone y beyond f32", zone("[0.5, 1e39]")),
        (
            "model override threshold",
            "[model_override]\nscore_threshold = nan\n".to_string(),
        ),
        (
            "ensemble member threshold",
            "[model_override]\nkind = \"ensemble\"\n\n[[model_override.members]]\n\
                 score_threshold = -inf\n"
                .to_string(),
        ),
    ] {
        let mut cfg = Config::default();
        cfg.cameras.push(
            toml::from_str(&format!(
                "id = 1\nname = \"c1\"\nurl = \"rtsp://example/cam\"\n\n{cam_src}"
            ))
            .unwrap(),
        );
        let err = cfg.validate().expect_err(&format!(
            "{field}: a number JSON cannot hold must be refused"
        ));
        assert!(
            err.to_string().contains("camera 1") && err.to_string().contains("finite"),
            "{field}: {err}"
        );
    }

    let mut cfg = Config::default();
    cfg.cameras.push(
        toml::from_str(&format!(
            "id = 1\nname = \"c1\"\nurl = \"rtsp://example/cam\"\n\n{}\n\
                 [model_override]\nscore_threshold = 0.4\n",
            zone("[0.5, 0.5]"),
        ))
        .unwrap(),
    );
    cfg.validate()
        .expect("finite numbers in every camera float must validate");
}

/// The default model is stored too: `PUT /admin/inference/model` merges
/// its patch onto the boot model and persists the result as JSON, where
/// a threshold JSON cannot hold becomes `null` and the saved model no
/// longer reads back. So load refuses one there as well.
#[test]
fn a_default_model_number_json_cannot_hold_is_refused_at_load() {
    let mut cfg = Config::default();
    cfg.inference.model.members.push(ModelConfig {
        score_threshold: f32::NAN,
        ..ModelConfig::default()
    });
    let err = cfg
        .validate()
        .expect_err("a member threshold JSON cannot hold must be refused");
    assert!(
        err.to_string().contains("inference.model") && err.to_string().contains("finite"),
        "{err}"
    );
}
