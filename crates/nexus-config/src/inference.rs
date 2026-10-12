//! Inference

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceConfig {
    /// Single-process or pool of N workers.
    #[serde(default)]
    pub backend: InferenceBackendKind,
    /// Pool-worker isolation strategy. Ignored when `backend != pool`.
    #[serde(default)]
    pub pool_worker_kind: PoolWorkerKind,
    #[serde(default = "default_workers")]
    pub workers: usize,
    #[serde(default = "default_restart_backoff_ms")]
    pub restart_backoff_ms: u64,
    /// On all-workers-down, fall through to in-process backend.
    #[serde(default = "default_true")]
    pub fail_soft: bool,
    /// Ordered list of EPs to try at session-init time.
    #[serde(default = "default_ep_priority")]
    pub ep_priority: Vec<String>,
    /// ORT intra-op thread-pool size, **per session**.
    ///
    /// `None` (the default) auto-sizes: `cores / ort_session_count()`
    /// when the session lands on the CPU EP, or a small fixed pool
    /// when an accelerator EP (OpenVINO / NPU / GPU / CUDA / TensorRT
    /// / CoreML / Vulkan) attached and the CPU pool only services
    /// fallback nodes.
    ///
    /// Left unset, ORT defaults this to the machine's logical core
    /// count *for every session independently*, which massively
    /// oversubscribes a box that also runs N camera pipelines — the
    /// threads then spend most of their time in `SpinPause()` at
    /// fork-join barriers rather than doing arithmetic.
    #[serde(default)]
    pub ort_intra_threads: Option<usize>,
    /// Let ORT thread-pool workers busy-wait before blocking.
    ///
    /// **Disabled by default**, which is the opposite of ORT's own
    /// default. Spinning only pays off when a session owns the
    /// machine and runs inferences back-to-back. Here the engine also
    /// runs GStreamer decode/convert threads for every camera, so a
    /// spinning ORT worker steals the core its own preempted peer
    /// needs to reach the barrier — the spin actively prolongs the
    /// wait it is spinning on. Enable only on a dedicated box.
    #[serde(default)]
    pub ort_allow_spinning: bool,
    /// Concrete model (open-vocab, ensemble, …).
    #[serde(default)]
    pub model: ModelConfig,
}

impl InferenceConfig {
    /// How many ORT sessions the detector layer will open in this
    /// process for this config.
    ///
    /// `backend = "pool"` builds one detector per worker **plus** one
    /// more for the `fail_soft` in-process fallback (see
    /// `nexus_inference::build`). Each of those is an independent ORT
    /// session with its own intra-op thread pool, so this is the
    /// divisor for auto-sizing those pools.
    ///
    /// `pool_worker_kind = "process"` is excluded: those detectors are
    /// built inside the child `nexus-inference-worker` processes, each
    /// of which sees a single session.
    pub fn ort_session_count(&self) -> usize {
        match self.backend {
            InferenceBackendKind::InProcess => 1,
            InferenceBackendKind::Pool => match self.pool_worker_kind {
                PoolWorkerKind::Process => 1,
                PoolWorkerKind::Thread => self.workers.max(1) + usize::from(self.fail_soft),
            },
        }
    }
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            backend: InferenceBackendKind::default(),
            pool_worker_kind: PoolWorkerKind::default(),
            workers: default_workers(),
            restart_backoff_ms: default_restart_backoff_ms(),
            fail_soft: true,
            ep_priority: default_ep_priority(),
            ort_intra_threads: None,
            ort_allow_spinning: false,
            model: ModelConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InferenceBackendKind {
    /// Single in-process detector.
    #[default]
    InProcess,
    /// `DetectorPool` of N backends + fail-soft fallback.
    Pool,
}

/// Isolation strategy for backends inside a `DetectorPool`.
///
/// `Thread` is the dev / single-host default: each worker is an OS thread
/// with its own current-thread tokio runtime. Cheap to spin up, shares
/// address space with the engine.
///
/// `Process` spawns the `nexus-inference-worker` binary as a child and
/// drives it over a length-prefixed bincode pipe. This is the production
/// stance — a panicking model or driver bug only takes the child down,
/// the engine + pool route around the dead slot, and the fail-soft
/// fallback keeps the pipeline live until M2's in-place restart lands.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolWorkerKind {
    #[default]
    Thread,
    Process,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// `"yolo"` (closed-vocab YOLOv26-nano, default) | `"open_vocab"` /
    /// `"yolo_world"` | `"yoloe"` (M3.1 open-vocab text + visual prompts) |
    /// `"yoloe_visual"` | `"yoloe_promptfree"` (M3.3 open-set auto-class)
    /// | `"ensemble"` (M3.2 same-camera multi-detector fan-out — see
    /// `members` below) | `"mock"`.
    ///
    /// `"classifier_ensemble"` / `"ppe"` are still accepted, but no model
    /// ships for them: they resolve to the degraded detector, report zero
    /// detections, and mark engine health degraded.
    ///
    /// `yolo` matches the v1 ship — `models/yolo26n_<W>x<H>.onnx` on the
    /// native 16:9 ladder (512x288 … 1536x864).
    #[serde(default = "default_model_kind")]
    pub kind: String,
    /// Optional model-pack directory containing `models-manifest.json`.
    /// When set, the engine loads the shape-matched artifact
    /// (`<model>_<W>x<H>.*`) from it, keyed on `input_width` /
    /// `input_height`.
    ///
    /// `skip_serializing_if` keeps `None` out of the serialized form so the
    /// fleet-config hash (`nexus-engine` `fleet_hash`) matches the cloud's
    /// `normalize_detector_config` projection, which drops an absent
    /// `pack_path` rather than emitting `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_path: Option<PathBuf>,
    /// Pack preset label — the canonical `"<W>x<H>"` shape string (e.g.
    /// "512x288"). A display/label mirror of `input_width`×`input_height`;
    /// the resolver keys on those two fields, not this string. Legacy
    /// bare-width presets ("640") are normalized at load.
    #[serde(default = "default_preset")]
    pub preset: String,
    #[serde(default = "default_input_width")]
    pub input_width: u32,
    #[serde(default = "default_input_height")]
    pub input_height: u32,
    #[serde(default = "default_score_threshold")]
    pub score_threshold: f32,
    /// M3.2 — same-camera detector ensemble. Meaningful when
    /// `kind == "ensemble"`: each entry is itself a `ModelConfig`
    /// (so members can be `yolo`, `yolo_world`, `yoloe`,
    /// `yoloe_visual`, or even another nested `ensemble`). Per-member
    /// fields like `pack_path`, `preset`, `input_width`,
    /// `input_height`, `score_threshold` apply to that member only;
    /// the parent's values are ignored when `kind == "ensemble"`.
    /// Omitted / empty under any other `kind` (kept opt-in via
    /// `serde(default)` so existing configs round-trip unchanged).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ModelConfig>,
    /// Per-frame cap on detections returned by **any** detector kind.
    /// `None` keeps every detection that survives the inner pipeline;
    /// `Some(k)` sorts by confidence desc and truncates to the K most-
    /// confident objects. Wired at construction time via
    /// [`crate::caps::TopKDetector`] — see
    /// `crates/nexus-inference/src/caps.rs`.
    ///
    /// History: this field originated as the M3.3 yoloe_promptfree-only
    /// cap and was promoted to a universal knob in M_PERF_CROWD Phase B1
    /// without renaming so existing configs round-trip unchanged. The
    /// open-vocab `yoloe_promptfree` kind also applies it internally
    /// (its baseline behaviour); the outer wrapper is idempotent in
    /// that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
    /// M_PERF_CROWD Phase B1 — drop any detection whose bbox area
    /// (`(x2 − x1) × (y2 − y1)` in supervisor-frame pixels) is below
    /// this threshold. Primary far-field noise knob for closed-vocab
    /// `yolo` on wide-angle lenses. `None` disables (current
    /// behaviour). Per-zone tighter overrides land via
    /// [`ZoneConfig::min_bbox_area_px_override`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_bbox_area_px: Option<u32>,
    /// M_PERF_CROWD Phase C3 — opt-in spatial bucketing for the
    /// shared class-aware NMS pass in `nexus_inference::nms`. Used by
    /// `yoloe`, `yoloe_visual`, `yolo_world`, and `ensemble`. Closes
    /// the O(N²) suppression scan to O(N) by only checking the 3×3
    /// grid neighbourhood. Output is bit-identical to the naive path
    /// when `bucket_size_px ≥ max bbox dim in supervisor frame`.
    /// `None` (default) preserves the pre-C3 naive pass exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nms_spatial_bucket_size_px: Option<u32>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            kind: default_model_kind(),
            pack_path: None,
            preset: default_preset(),
            input_width: default_input_width(),
            input_height: default_input_height(),
            score_threshold: default_score_threshold(),
            members: Vec::new(),
            top_k: None,
            min_bbox_area_px: None,
            nms_spatial_bucket_size_px: None,
        }
    }
}

impl ModelConfig {
    /// Remap a legacy square (or off-ladder) input shape to the native
    /// 16:9 ladder, in place, recursively for ensemble members. Emits a
    /// `warn!` per remap; never fails. Also normalizes `preset` to the
    /// canonical `"<W>x<H>"` form the (w,h)-keyed resolver expects, so
    /// legacy bare-width presets ("640") stop shadowing the real shape.
    pub fn remap_legacy_shapes(&mut self) {
        if let Some((w, h)) = remap_to_ladder(self.input_width, self.input_height) {
            let from = format!("{}x{}", self.input_width, self.input_height);
            let to = format!("{w}x{h}");
            warn!(
                %from,
                %to,
                kind = %self.kind,
                "config: remapped legacy detector input shape to the native 16:9 ladder"
            );
            self.input_width = w;
            self.input_height = h;
        }
        let canonical = format!("{}x{}", self.input_width, self.input_height);
        if self.preset != canonical {
            self.preset = canonical;
        }
        for member in &mut self.members {
            member.remap_legacy_shapes();
        }
    }

    /// Refuse a `score_threshold`, here or in an ensemble member, that is
    /// not a finite number. See [`CameraConfig::validate_finite`].
    pub fn validate_finite(&self) -> Result<(), String> {
        if !self.score_threshold.is_finite() {
            return Err(format!(
                "score_threshold must be a finite number (got {})",
                self.score_threshold
            ));
        }
        self.members.iter().try_for_each(Self::validate_finite)
    }
}

fn default_workers() -> usize {
    1
}
fn default_restart_backoff_ms() -> u64 {
    2_000
}
pub(crate) fn default_true() -> bool {
    true
}
/// Default EP order matches the documented hardware matrix:
///   Intel iGPU/dGPU/NPU → openvino
///   NVIDIA              → tensorrt → cuda (M5)
///   anything else       → cpu
/// A generated `/etc/nexus/nexus.toml` (`nexus-probe emit-config`) pins the
/// right short list for the detected box (e.g. an NPU box adds "npu" between
/// openvino and cpu; an NVIDIA box leads with "tensorrt"). `coreml` is
/// dev-only and excluded from production defaults — opt in explicitly in your
/// config if you need it.
pub(crate) fn default_ep_priority() -> Vec<String> {
    vec![
        "openvino".into(),
        "tensorrt".into(),
        "cuda".into(),
        "cpu".into(),
    ]
}
fn default_model_kind() -> String {
    "yolo".into()
}
fn default_preset() -> String {
    "512x288".into()
}
fn default_input_width() -> u32 {
    512
}
fn default_input_height() -> u32 {
    288
}
fn default_score_threshold() -> f32 {
    0.30
}

/// The native-16:9 shape ladder — exact 16:9 ∩ stride-32 (W=512k, H=288k).
/// Every shipped detector input shape is one of these rungs.
pub const SHAPE_LADDER: [(u32, u32); 3] = [(512, 288), (1024, 576), (1536, 864)];

/// Map a legacy square (or otherwise off-ladder) `(w, h)` to the nearest
/// native-16:9 ladder rung. The three shipped legacy squares map by
/// intent (640→512×288, 960→1024×576, 1280→1536×864); anything else
/// snaps to the rung closest in pixel count. Returns `None` when the
/// shape is already an exact ladder rung.
fn remap_to_ladder(w: u32, h: u32) -> Option<(u32, u32)> {
    if SHAPE_LADDER.contains(&(w, h)) {
        return None;
    }
    Some(match (w, h) {
        (640, 640) => (512, 288),
        (960, 960) => (1024, 576),
        (1280, 1280) => (1536, 864),
        _ => {
            let target = u64::from(w) * u64::from(h);
            *SHAPE_LADDER
                .iter()
                .min_by_key(|(lw, lh)| (u64::from(*lw) * u64::from(*lh)).abs_diff(target))
                .expect("SHAPE_LADDER is non-empty")
        }
    })
}
