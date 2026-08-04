//! Phase 7.5 · Step 7.5.5 — canonical hashing of fleet-settings state.
//!
//! The edge periodically reports one SHA-256 per fleet-settings category
//! to the cloud (`core_state_hashes` envelope) so the console can render
//! configuration drift without round-tripping the full config. The hash
//! must be **byte-identical** to the cloud's own projection of the
//! effective fleet payload, so this module mirrors the cloud's
//! `canonical_json` + `sha256_hex` exactly (see
//! `nexus-cloud-console/services/api-gateway/src/handlers/fleet.rs`).
//!
//! # Canonical category contract
//!
//! The per-category canonical value is defined so the edge reports the
//! single fleet-shaped value when (and only when) it is uniformly
//! applied:
//!
//! * **rules** — JSON array of full rule objects, sorted by `id`. `None`
//!   when the edge has no rules.
//! * **text_prompts** — the common ordered prompt list shared by *every*
//!   camera. `None` when there are no cameras, the cameras disagree, or
//!   the shared list is empty.
//! * **visual_prompts** — the sorted list of attached prompt *names*
//!   shared by *every* camera. `None` when there are no cameras, the
//!   cameras disagree, or the shared set is empty.
//! * **detector_config** — the model-override object shared by *every*
//!   camera. `None` when there are no cameras or any camera lacks the
//!   override / disagrees.
//! * **delivery_settings** — `{enabled, schedule, timezone}` (the
//!   `updated_at` field is excluded — the cloud has no equivalent).
//!   Always present (the edge seeds a singleton row).
//!
//! When a per-camera category is non-uniform the hash is `None`, which
//! the cloud renders as drift against its single projected value.
//!
//! # Merge-managed subsets (Phase 7.5.11)
//!
//! Under `replace` the edge's whole state for a category *is* the fleet
//! payload, so hashing everything is right. Under `merge` it is not: the
//! edge deliberately keeps operator-local entries the cloud has never
//! seen, so a whole-state digest could never equal the cloud's
//! projection of the effective payload and the console would show
//! permanent, un-clearable drift (and re-push on every auto-apply tick).
//!
//! So for a list-shaped category whose marker says `mode = 'merge'`, the
//! digest is taken over **only the keys that marker records as
//! fleet-managed** — the exact set the cloud projected. Local-only
//! entries are invisible to drift, which is precisely the contract
//! `merge` promises. If an operator edits or deletes a *fleet-managed*
//! entry the subset changes and drift still fires. This is what lets the
//! whole feature ship with no cloud-side hashing change.

use std::collections::BTreeSet;

use nexus_store::Store;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The five per-category canonical hashes. A `None` field means the edge
/// has no (uniform) state for that category.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CategoryHashes {
    pub rules: Option<String>,
    pub text_prompts: Option<String>,
    pub visual_prompts: Option<String>,
    pub detector_config: Option<String>,
    pub delivery_settings: Option<String>,
}

/// Compute the per-category canonical hashes from the live store state.
pub async fn compute(store: &Store) -> anyhow::Result<CategoryHashes> {
    let managed = MergeManaged::load(store).await;
    let rules = hash_rules(store, managed.keys("rules")).await?;
    let cameras = store.list_cameras().await?;
    let text_prompts = hash_text_prompts(&cameras, managed.keys("text_prompts"));
    let detector_config = hash_detector_config(&cameras);
    let visual_prompts =
        hash_visual_prompts(store, &cameras, managed.keys("visual_prompts")).await?;
    let delivery_settings = hash_delivery_settings(store).await?;
    Ok(CategoryHashes {
        rules,
        text_prompts,
        visual_prompts,
        detector_config,
        delivery_settings,
    })
}

/// The fleet-managed key sets for categories last applied with `merge`.
///
/// A category is absent when it was never fleet-applied or was applied
/// with `replace` — in both cases the digest covers the whole local
/// state, as it did before Phase 7.5.11.
#[derive(Debug, Default)]
struct MergeManaged(std::collections::HashMap<String, Vec<String>>);

impl MergeManaged {
    /// Reads the markers. A read failure degrades to "no merge-managed
    /// categories", i.e. whole-state digests — the pre-7.5.11 behaviour,
    /// which at worst over-reports drift rather than hiding it.
    async fn load(store: &Store) -> Self {
        let markers = match store.list_fleet_managed_markers().await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "fleet markers unreadable; hashing whole local state");
                return Self::default();
            }
        };
        Self(
            markers
                .into_iter()
                .filter(|m| m.mode.as_deref() == Some("merge"))
                .filter_map(|m| m.managed_keys.map(|keys| (m.category, keys)))
                .collect(),
        )
    }

    fn keys(&self, category: &str) -> Option<&[String]> {
        self.0.get(category).map(Vec::as_slice)
    }
}

/// `rules` — array of full rule objects sorted by `id`.
///
/// When `managed` is `Some`, only rules the fleet owns are hashed; local
/// rules are excluded so the digest matches the cloud's projection.
async fn hash_rules(store: &Store, managed: Option<&[String]>) -> anyhow::Result<Option<String>> {
    let mut rules = store.list_rules().await?;
    if let Some(managed) = managed {
        let owned: std::collections::HashSet<&str> = managed.iter().map(String::as_str).collect();
        rules.retain(|r| owned.contains(r.id.as_str()));
    }
    if rules.is_empty() {
        return Ok(None);
    }
    rules.sort_by(|a, b| a.id.cmp(&b.id));
    let arr = Value::Array(
        rules
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?,
    );
    Ok(Some(sha256_canonical(&arr)))
}

/// `text_prompts` — the ordered prompt list shared by every camera.
///
/// When `managed` is `Some`, the projection is the managed list **in the
/// order the cloud sent it**, narrowed to the prompts every camera still
/// carries. Dropping one on any camera changes the digest, so an
/// operator deleting a fleet-pushed prompt still shows as drift, while
/// their own prompts stay invisible to it.
fn hash_text_prompts(
    cameras: &[nexus_config::CameraConfig],
    managed: Option<&[String]>,
) -> Option<String> {
    let first = cameras.first()?;
    let shared: Vec<String> = match managed {
        None => {
            let shared = &first.detector.prompts;
            if !cameras.iter().all(|c| &c.detector.prompts == shared) {
                return None;
            }
            shared.clone()
        }
        Some(managed) => managed
            .iter()
            .filter(|p| cameras.iter().all(|c| c.detector.prompts.contains(p)))
            .cloned()
            .collect(),
    };
    if shared.is_empty() {
        return None;
    }
    let arr = Value::Array(shared.into_iter().map(Value::String).collect());
    Some(sha256_canonical(&arr))
}

/// `detector_config` — the model-override object shared by every camera.
fn hash_detector_config(cameras: &[nexus_config::CameraConfig]) -> Option<String> {
    if cameras.is_empty() {
        return None;
    }
    // Canonicalize each camera's override; the category is defined only
    // when every camera has the same `Some(model_override)`.
    let mut shared: Option<String> = None;
    for cam in cameras {
        let model = cam.detector.model_override.as_ref()?;
        // Serialize via string (not `to_value`) so `serde_json`'s float
        // formatter emits the shortest round-tripping decimal for the
        // `f32` `score_threshold` (`0.3`, not the `0.30000001192092896`
        // that an `f32`->`f64` `to_value` widening produces) and
        // `skip_serializing_if` drops the null / empty optionals. This
        // MUST match the cloud's `normalize_detector_config` projection
        // (api-gateway `handlers/fleet.rs`).
        let json = serde_json::to_string(model).ok()?;
        let canon = canonical_json(&serde_json::from_str::<Value>(&json).ok()?);
        match &shared {
            None => shared = Some(canon),
            Some(prev) if *prev == canon => {}
            Some(_) => return None, // cameras disagree -> drift
        }
    }
    shared.map(|canon| sha256_hex(&canon))
}

/// `visual_prompts` — the sorted attached-prompt-name set shared by every
/// camera. When `managed` is `Some`, only fleet-owned names count toward
/// the set, so locally-attached prompts do not read as drift.
async fn hash_visual_prompts(
    store: &Store,
    cameras: &[nexus_config::CameraConfig],
    managed: Option<&[String]>,
) -> anyhow::Result<Option<String>> {
    if cameras.is_empty() {
        return Ok(None);
    }
    let owned: Option<std::collections::HashSet<&str>> =
        managed.map(|m| m.iter().map(String::as_str).collect());
    // id -> name for every visual prompt on this core.
    let summaries = store.list_visual_prompts().await?;
    let name_of: std::collections::HashMap<_, _> =
        summaries.iter().map(|s| (s.id, s.name.clone())).collect();

    let mut shared: Option<BTreeSet<String>> = None;
    for cam in cameras {
        let ids = store.list_camera_visual_prompt_ids(cam.id).await?;
        let names: BTreeSet<String> = ids
            .iter()
            .filter_map(|id| name_of.get(id).cloned())
            .filter(|name| owned.as_ref().is_none_or(|o| o.contains(name.as_str())))
            .collect();
        match &shared {
            None => shared = Some(names),
            Some(prev) if *prev == names => {}
            Some(_) => return Ok(None), // cameras disagree -> drift
        }
    }
    let shared = shared.unwrap_or_default();
    if shared.is_empty() {
        return Ok(None);
    }
    let arr = Value::Array(shared.into_iter().map(Value::String).collect());
    Ok(Some(sha256_canonical(&arr)))
}

/// `delivery_settings` — `{enabled, schedule, timezone}` (no `updated_at`).
async fn hash_delivery_settings(store: &Store) -> anyhow::Result<Option<String>> {
    let s = store.delivery_settings_get().await?;
    let value = serde_json::json!({
        "enabled": s.enabled,
        "schedule": s.schedule,
        "timezone": s.timezone,
    });
    Ok(Some(sha256_canonical(&value)))
}

/// Lower-hex SHA-256 of the canonical JSON of `value`. This is the public
/// entry point both the emit path and the fleet-apply marker use.
pub fn sha256_canonical(value: &Value) -> String {
    sha256_hex(&canonical_json(value))
}

/// Deterministic, key-sorted JSON serialization. MUST byte-match the
/// cloud's `canonical_json` (api-gateway `handlers/fleet.rs`).
fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => push_json_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_string(k, out);
                out.push(':');
                write_canonical(&map[*k], out);
            }
            out.push('}');
        }
    }
}

fn push_json_string(s: &str, out: &mut String) {
    match serde_json::to_string(s) {
        Ok(encoded) => out.push_str(&encoded),
        Err(_) => out.push_str("\"\""),
    }
}

/// Lower-hex SHA-256 of a string.
fn sha256_hex(s: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(s.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    use nexus_config::StoreConfig;
    use nexus_config::{CameraConfig, ModelConfig};
    use serde_json::json;

    fn model(preset: &str) -> ModelConfig {
        let (w, h) = preset
            .split_once('x')
            .map(|(a, b)| (a.parse::<u32>().unwrap(), b.parse::<u32>().unwrap()))
            .expect("preset must be WxH");
        serde_json::from_value(json!({
            "kind": "yolo",
            "preset": preset,
            "input_width": w,
            "input_height": h,
        }))
        .expect("model config")
    }

    fn camera(
        id: nexus_types::CameraId,
        prompts: &[&str],
        model_override: Option<ModelConfig>,
    ) -> CameraConfig {
        CameraConfig {
            id,
            name: format!("cam{id}"),
            ingest: nexus_config::CameraIngest {
                url: url::Url::parse("rtsp://127.0.0.1/stream").unwrap(),
                enabled: true,
                max_fps: 0,
                codec: None,
            },
            detector: nexus_config::CameraDetector {
                prompts: prompts.iter().map(|p| (*p).to_string()).collect(),
                visual_prompts: vec![],
                model_override,
            },
            behavior: nexus_config::CameraBehavior {
                parking_lot_mode: false,
                anchor_ttl_secs: None,
                ..Default::default()
            },
            onvif: Default::default(),
            talk_down: Default::default(),
            zones: vec![],
        }
    }

    async fn open_store() -> Store {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nexus.db");
        let store = Store::open(&StoreConfig {
            url: format!("sqlite:{}?mode=rwc", db_path.display()),
            seed_from_config: false,
            duckdb_attach: false,
            duckdb_path: std::path::PathBuf::from("/tmp/unused.duckdb"),
        })
        .await
        .unwrap();
        // Keep the tempdir alive for the lifetime of the store by
        // leaking it — the test process is short-lived.
        std::mem::forget(dir);
        store
    }

    /// `canonical_json` must be insensitive to object key insertion
    /// order (it sorts keys) and stable for arrays (document order).
    #[test]
    fn canonical_json_is_key_order_independent() {
        let a = json!({ "b": 1, "a": [1, 2, { "z": true, "y": null }] });
        let b = json!({ "a": [1, 2, { "y": null, "z": true }], "b": 1 });
        assert_eq!(sha256_canonical(&a), sha256_canonical(&b));
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }

    /// Frozen cross-repo vector. This MUST stay byte-identical to the
    /// cloud's `canonical_json` / `sha256_hex` (api-gateway
    /// `handlers/fleet.rs`). The cloud carries the SAME constant in its
    /// fleet-handler tests; if either side changes the algorithm one of
    /// the two frozen hashes breaks, surfacing the drift immediately.
    #[test]
    fn canonical_json_frozen_vector() {
        let v = json!({
            "enabled": true,
            "schedule": null,
            "timezone": "UTC",
            "nested": { "b": 2, "a": 1 },
            "list": ["x", "y"],
        });
        assert_eq!(
            canonical_json(&v),
            r#"{"enabled":true,"list":["x","y"],"nested":{"a":1,"b":2},"schedule":null,"timezone":"UTC"}"#
        );
        assert_eq!(
            sha256_canonical(&v),
            "c9e57d7abdf709975edb7a8530d071c0cb0bd00e2019f9efa9adc798b5a41270"
        );
    }

    /// `text_prompts` reduces to a hash only when every camera shares
    /// the identical (non-empty, order-significant) prompt list.
    #[test]
    fn text_prompts_uniform_vs_divergent() {
        // No cameras → no shared list.
        assert!(hash_text_prompts(&[], None).is_none());

        // Empty prompts on the only camera → None.
        assert!(hash_text_prompts(&[camera(1, &[], None)], None).is_none());

        // Two cameras agreeing → Some, stable.
        let uniform = [
            camera(1, &["person", "car"], None),
            camera(2, &["person", "car"], None),
        ];
        let h = hash_text_prompts(&uniform, None).expect("uniform → Some");
        assert_eq!(hash_text_prompts(&uniform, None), Some(h.clone()));

        // Order matters: a reordered list is a different category value.
        let reordered = [camera(1, &["car", "person"], None)];
        assert_ne!(hash_text_prompts(&reordered, None), Some(h));

        // Cameras disagreeing → None (= drift).
        let divergent = [
            camera(1, &["person"], None),
            camera(2, &["person", "car"], None),
        ];
        assert!(hash_text_prompts(&divergent, None).is_none());
    }

    /// Phase 7.5.11 — a merge-managed `text_prompts` category digests
    /// only the fleet-owned prompts, so operator-local prompts (and
    /// cameras disagreeing *about local prompts*) never read as drift,
    /// while the digest still equals the cloud's projection of the
    /// effective payload.
    #[test]
    fn text_prompts_merge_managed_subset_ignores_local() {
        let managed: Vec<String> = vec!["person".into(), "car".into()];

        // The cloud's projection: exactly the managed list, in order.
        let fleet_only = [camera(1, &["person", "car"], None)];
        let cloud_view = hash_text_prompts(&fleet_only, None).expect("fleet-only → Some");

        // Same core after the operator adds a local prompt to one camera
        // and a different one to the other. Under `replace` semantics
        // this is drift twice over (non-uniform, and ≠ cloud). Under
        // `merge` it hashes to exactly the cloud's projection.
        let with_local = [
            camera(1, &["person", "car", "my-dog"], None),
            camera(2, &["person", "car", "delivery-van"], None),
        ];
        assert!(hash_text_prompts(&with_local, None).is_none());
        assert_eq!(
            hash_text_prompts(&with_local, Some(&managed)),
            Some(cloud_view.clone())
        );

        // Deleting a *fleet-owned* prompt from one camera still drifts.
        let fleet_prompt_deleted = [
            camera(1, &["person", "my-dog"], None),
            camera(2, &["person", "car"], None),
        ];
        assert_ne!(
            hash_text_prompts(&fleet_prompt_deleted, Some(&managed)),
            Some(cloud_view)
        );

        // A merge push of nothing owns nothing → no category value.
        assert!(hash_text_prompts(&with_local, Some(&[])).is_none());
    }

    /// `detector_config` reduces to a hash only when every camera shares
    /// an identical model override.
    #[test]
    fn detector_config_uniform_vs_divergent() {
        // No cameras → None.
        assert!(hash_detector_config(&[]).is_none());

        // A camera without an override → None.
        assert!(hash_detector_config(&[camera(1, &[], None)]).is_none());

        // Two cameras with the same override → Some.
        let uniform = [
            camera(1, &[], Some(model("512x288"))),
            camera(2, &[], Some(model("512x288"))),
        ];
        assert!(hash_detector_config(&uniform).is_some());

        // Different presets → None (= drift).
        let divergent = [
            camera(1, &[], Some(model("512x288"))),
            camera(2, &[], Some(model("1536x864"))),
        ];
        assert!(hash_detector_config(&divergent).is_none());
    }

    /// FROZEN cross-repo vector. The default detector override
    /// (`{kind:"yolo", 512×288}`, every other field defaulted)
    /// canonicalizes to the shape the cloud's `normalize_detector_config`
    /// produces and hashes to this exact SHA. It MUST stay byte-identical
    /// to the cloud's `project_runtime_sha(Category::DetectorConfig, …)`
    /// frozen vector in api-gateway `handlers/fleet.rs`
    /// (`detector_config_projection_normalizes_to_edge_form`). If either
    /// side's detector canonicalization drifts — e.g. the `f32`
    /// `score_threshold` starts widening again, or `pack_path` stops being
    /// skipped — one of the two frozen hashes breaks and surfaces it.
    #[test]
    fn detector_config_default_frozen_vector() {
        // The `ModelConfig` round-trip drops `pack_path` / `members` /
        // caps and emits the `f32` `score_threshold` as its shortest
        // decimal (`0.3`).
        let json = serde_json::to_string(&model("512x288")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            canonical_json(&value),
            r#"{"input_height":288,"input_width":512,"kind":"yolo","preset":"512x288","score_threshold":0.3}"#
        );

        let uniform = [
            camera(1, &[], Some(model("512x288"))),
            camera(2, &[], Some(model("512x288"))),
        ];
        assert_eq!(
            hash_detector_config(&uniform),
            Some("5c14a4012b1233ed210e7cdeb28a629e7bfe20a85e37db0338bd4d0c245dd142".to_owned()),
        );
    }

    /// On a fresh store: no rules, no cameras → both `None`; delivery
    /// is always present (seeded singleton).
    #[tokio::test]
    async fn compute_on_empty_store() {
        let store = open_store().await;
        let hashes = compute(&store).await.unwrap();
        assert!(hashes.rules.is_none());
        assert!(hashes.text_prompts.is_none());
        assert!(hashes.visual_prompts.is_none());
        assert!(hashes.detector_config.is_none());
        assert!(
            hashes.delivery_settings.is_some(),
            "delivery settings singleton is always reported"
        );
    }

    /// `text_prompts` from the live store reflects a uniform fleet
    /// applied across every camera.
    #[tokio::test]
    async fn compute_reflects_uniform_text_prompts() {
        let store = open_store().await;
        store
            .upsert_camera(&camera(1, &["person"], None))
            .await
            .unwrap();
        store
            .upsert_camera(&camera(2, &["person"], None))
            .await
            .unwrap();
        let uniform = compute(&store).await.unwrap();
        assert!(uniform.text_prompts.is_some());

        // Diverge one camera → the category drops to drift (`None`).
        store
            .upsert_camera(&camera(2, &["car"], None))
            .await
            .unwrap();
        let divergent = compute(&store).await.unwrap();
        assert!(divergent.text_prompts.is_none());
    }
}
