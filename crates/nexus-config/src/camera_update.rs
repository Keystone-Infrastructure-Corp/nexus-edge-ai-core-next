//! CameraConfigUpdate — what gets fan-pushed to detector slots on hot reload

use crate::cameras::VisualPromptRef;
use crate::inference::ModelConfig;
use nexus_types::CameraId;
use serde::{Deserialize, Serialize};

/// Diff sent into every detector slot when a camera changes. Each slot
/// applies it idempotently — if the diff matches its current state the
/// push is a no-op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfigUpdate {
    pub camera_id: CameraId,
    pub prompts: Vec<String>,
    /// M3.1 — visual-prompt attachments for this camera. Empty for
    /// every backend except the YOLOE visual-mode detector. Defaults
    /// to empty so older fan-push payloads that predate the field
    /// still deserialise cleanly.
    #[serde(default)]
    pub visual_prompts: Vec<VisualPromptRef>,
    pub model: ModelConfig,
    pub generation: u64,
}
