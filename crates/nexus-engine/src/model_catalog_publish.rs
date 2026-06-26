//! Phase 7.5 (wedge follow-up) — publish the engine's resolved detector
//! vocabulary to the cloud.
//!
//! The console renders detector prompt suggestions (the chip strip for
//! closed-vocab kinds, the suggestion box for open-vocab kinds) from live
//! data the edge reports, instead of a hand-maintained mirror of the
//! engine's label map. The engine is the source of truth: it resolves
//! every detector kind's vocabulary at boot in
//! [`crate::models_catalog::build_catalog`] (closed-vocab COCO→domain map,
//! open-vocab baked manifest prompts) and we forward that snapshot to the
//! cloud as a `model_catalog` envelope.
//!
//! ### When we publish
//!
//! The catalog is a boot-time snapshot — it only changes across an OTA /
//! engine restart, which is a fresh process anyway. So there is nothing to
//! recompute at runtime; we simply push the snapshot on every tunnel-up
//! (the rising edge of [`TunnelOutbox::is_connected`]). Fire-and-forget:
//! the cloud upserts the catalog keyed on `core_id`, and the next report on
//! reconnect is the recovery path. Pre-feature cloud peers ignore the kind.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use nexus_cloud_client::TunnelOutbox;
use nexus_cloud_protocol::v1::{
    DetectorVocabEntry, Envelope, EnvelopeBody, EnvelopeMeta, ModelCatalogPayload,
};
use tokio::task::JoinHandle;
use tracing::debug;
use uuid::Uuid;

use crate::models_catalog::ModelPromptsCatalog;

/// Poll cadence for the tunnel-up edge detector. The catalog is static,
/// so a slow poll is fine — this only governs how quickly a fresh report
/// follows a (re)connect.
const TICK: Duration = Duration::from_secs(5);

/// Project the engine's in-memory catalog onto the wire payload. The
/// `groups` field is intentionally dropped — it is a UI-layout hint the
/// console recomputes locally; the wire only carries the flat vocabulary.
fn build_payload(catalog: &ModelPromptsCatalog) -> ModelCatalogPayload {
    let kinds = catalog
        .kinds
        .iter()
        .map(|k| DetectorVocabEntry {
            kind: k.kind.clone(),
            open_vocab: k.open_vocab,
            loaded: k.loaded,
            prompts: k.prompts.clone(),
            note: k.note.clone(),
        })
        .collect();
    ModelCatalogPayload {
        default_kind: catalog.default_kind.clone(),
        kinds,
        computed_at: Utc::now().to_rfc3339(),
    }
}

fn build_envelope(payload: ModelCatalogPayload) -> Envelope {
    Envelope {
        meta: EnvelopeMeta {
            v: 1,
            id: Uuid::now_v7().to_string(),
            ts: Utc::now().to_rfc3339(),
            in_reply_to: None,
            seq: None,
            trace: None,
        },
        body: EnvelopeBody::ModelCatalog(payload),
    }
}

/// Spawn the model-catalog publisher. Sends one `model_catalog` envelope
/// on each tunnel-up, starting with the first connection after boot.
/// Returns the join handle so the engine shutdown path can abort it
/// alongside the other long-lived tasks.
pub fn spawn(catalog: Arc<ModelPromptsCatalog>, outbox: Arc<TunnelOutbox>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut was_connected = false;
        loop {
            tokio::time::sleep(TICK).await;
            let connected = outbox.is_connected();
            // Rising edge only: publish once per (re)connect.
            if connected && !was_connected {
                let env = build_envelope(build_payload(&catalog));
                match outbox.send(env).await {
                    Ok(()) => {
                        debug!(
                            default_kind = %catalog.default_kind,
                            kinds = catalog.kinds.len(),
                            "model_catalog published",
                        );
                        // Latch only on a successful send so a failed
                        // publish re-attempts on the next tick.
                        was_connected = true;
                    }
                    Err(e) => {
                        debug!(error = %e, "model_catalog publish failed; will retry");
                    }
                }
            } else {
                was_connected = connected;
            }
        }
    })
}
