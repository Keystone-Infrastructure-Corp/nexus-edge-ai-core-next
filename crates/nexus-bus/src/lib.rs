//! Typed in-process (and optionally NATS) message bus.
//!
//! The bus carries **metadata only** — never frame pixels. Frames flow on
//! the `LatestFrameCache` side-channel (see ARCHITECTURE.md L7).

#![forbid(unsafe_code)]

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use parking_lot::RwLock;
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tokio_stream::StreamExt;

// ---------------------------------------------------------------------------
// Topics
// ---------------------------------------------------------------------------

pub mod topic {
    pub const FRAME_METADATA: &str = "frame.metadata";
    pub const ALERT_EVENT: &str = "alert.event";
    pub const CONFIG_CHANGED: &str = "config.changed";
    pub const PIPELINE_STATUS: &str = "pipeline.status";
    /// Watermark sampler crossed `panic_watermark_pct`. Payload is
    /// `StoragePanicEvent` (free pct + reason). Emitted ONCE per
    /// Ok→Panic transition (debounced); the matching exit event is
    /// emitted on Panic→Ok.
    pub const STORAGE_PANIC: &str = "storage.panic";
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum BusError {
    #[error("serialize: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("publish: no subscribers")]
    NoSubscribers,
    #[error("publish: backend closed")]
    Closed,
    #[error("subscribe: lagged by {0} messages")]
    Lagged(u64),
    #[error("backend not available: {0}")]
    BackendUnavailable(&'static str),
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

pub type DynStream =
    Pin<Box<dyn Stream<Item = Result<serde_json::Value, BusError>> + Send + 'static>>;

#[async_trait]
pub trait Bus: Send + Sync {
    async fn publish_raw(&self, topic: &str, payload: serde_json::Value) -> Result<(), BusError>;
    async fn subscribe_raw(&self, topic: &str) -> Result<DynStream, BusError>;
}

/// Typed convenience layer. Anything that implements [`Bus`] gets these for free.
#[async_trait]
pub trait BusExt: Bus {
    async fn publish<T: Serialize + Send + Sync>(
        &self,
        topic: &str,
        msg: &T,
    ) -> Result<(), BusError> {
        let v = serde_json::to_value(msg)?;
        self.publish_raw(topic, v).await
    }

    async fn subscribe<T>(
        &self,
        topic: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<T, BusError>> + Send>>, BusError>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let raw = self.subscribe_raw(topic).await?;
        let mapped = raw
            .map(|r| r.and_then(|v| serde_json::from_value::<T>(v).map_err(BusError::Serialize)));
        Ok(Box::pin(mapped))
    }
}

impl<T: Bus + ?Sized> BusExt for T {}

// ---------------------------------------------------------------------------
// BroadcastBus — default in-process implementation
// ---------------------------------------------------------------------------

pub struct BroadcastBus {
    capacity: usize,
    channels: RwLock<std::collections::HashMap<String, broadcast::Sender<serde_json::Value>>>,
}

impl BroadcastBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            channels: RwLock::new(Default::default()),
        }
    }

    fn sender_for(&self, topic: &str) -> broadcast::Sender<serde_json::Value> {
        if let Some(tx) = self.channels.read().get(topic) {
            return tx.clone();
        }
        let mut w = self.channels.write();
        w.entry(topic.to_string())
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .clone()
    }
}

#[async_trait]
impl Bus for BroadcastBus {
    async fn publish_raw(&self, topic: &str, payload: serde_json::Value) -> Result<(), BusError> {
        let tx = self.sender_for(topic);
        // Allow no-subscribers; ops bus events shouldn't fail because no one's listening.
        let _ = tx.send(payload);
        Ok(())
    }

    async fn subscribe_raw(&self, topic: &str) -> Result<DynStream, BusError> {
        let rx = self.sender_for(topic).subscribe();
        let s = BroadcastStream::new(rx).map(|r| match r {
            Ok(v) => Ok(v),
            Err(BroadcastStreamRecvError::Lagged(n)) => Err(BusError::Lagged(n)),
        });
        Ok(Box::pin(s))
    }
}

// ---------------------------------------------------------------------------
// NatsBus — feature-gated stub. Wired in M2.
// ---------------------------------------------------------------------------

#[cfg(feature = "nats")]
pub struct NatsBus {
    _url: String,
}

#[cfg(feature = "nats")]
#[async_trait]
impl Bus for NatsBus {
    async fn publish_raw(&self, _topic: &str, _payload: serde_json::Value) -> Result<(), BusError> {
        Err(BusError::BackendUnavailable(
            "nats backend not yet implemented (M2)",
        ))
    }
    async fn subscribe_raw(&self, _topic: &str) -> Result<DynStream, BusError> {
        Err(BusError::BackendUnavailable(
            "nats backend not yet implemented (M2)",
        ))
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

pub fn build_bus(cfg: &nexus_config::BusConfig) -> Arc<dyn Bus> {
    match cfg.backend {
        nexus_config::BusBackendKind::Broadcast => Arc::new(BroadcastBus::new(cfg.capacity)),
        nexus_config::BusBackendKind::Nats => {
            tracing::warn!(
                "nats backend selected but feature not enabled; falling back to broadcast"
            );
            Arc::new(BroadcastBus::new(cfg.capacity))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn broadcast_pub_sub_roundtrip() {
        let bus = BroadcastBus::new(8);
        let mut rx = bus.subscribe::<serde_json::Value>("t").await.unwrap();
        bus.publish("t", &serde_json::json!({"hello": "world"}))
            .await
            .unwrap();
        let v = rx.next().await.unwrap().unwrap();
        assert_eq!(v["hello"], "world");
    }
}
