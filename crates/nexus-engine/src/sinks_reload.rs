//! M7 cloud-managed sinks — bus subscriber that rebuilds the live
//! [`nexus_sinks::SinkRegistry`] when the admin API mutates the
//! `alert_sinks` table.
//!
//! Subscribes to [`topic::SINK_CONFIG_CHANGED`] and, on each signal,
//! re-reads the db sinks, merges them with the boot-time file sinks
//! (`nexus.toml` `[[sinks]]`), and atomically swaps the registry via
//! [`nexus_sinks::SinkRegistry::replace`]. db sinks win on
//! `<kind>:<name>` collision (see migration `0021_alert_sinks.sql`).
//!
//! Bus payloads are empty sentinels — the reload always re-reads the
//! store, so a Lagged subscriber that drops an intermediate signal
//! still converges as soon as the next signal arrives. A rebuild
//! failure (e.g. a sink kind the binary wasn't built with) logs and
//! leaves the previous registry in place rather than tearing down
//! delivery.

use std::sync::Arc;

use futures::StreamExt;
use nexus_bus::{topic, Bus, BusExt};
use nexus_config::SinkConfig;
use nexus_sinks::SinkRegistry;
use nexus_store::Store;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Spawn the reload task. Returns its `JoinHandle` and a oneshot
/// `Sender` the main shutdown path uses to ask the task to exit. The
/// subscribe is best-effort: a failure logs once and the task exits
/// cleanly (manual API actions still update the db; only the *hot*
/// reload is lost, not the data).
pub fn spawn(
    bus: Arc<dyn Bus>,
    store: Arc<Store>,
    registry: Arc<SinkRegistry>,
    file_sinks: Arc<Vec<SinkConfig>>,
) -> (JoinHandle<()>, oneshot::Sender<()>) {
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let handle =
        tokio::spawn(async move { run(bus, store, registry, file_sinks, shutdown_rx).await });
    (handle, shutdown_tx)
}

async fn run(
    bus: Arc<dyn Bus>,
    store: Arc<Store>,
    registry: Arc<SinkRegistry>,
    file_sinks: Arc<Vec<SinkConfig>>,
    shutdown: oneshot::Receiver<()>,
) {
    let mut stream = match bus
        .subscribe::<serde_json::Value>(topic::SINK_CONFIG_CHANGED)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            error!(
                error = %e,
                "M7 sinks reload: failed to subscribe to sink.config.changed; hot reload disabled"
            );
            return;
        }
    };
    info!("M7 sinks reload: subscribed to sink.config.changed");

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!("M7 sinks reload: shutdown requested");
                return;
            }
            msg = stream.next() => {
                match msg {
                    None => {
                        warn!("M7 sinks reload: sink.config.changed stream ended");
                        return;
                    }
                    Some(Err(e)) => {
                        // Lagged subscriber — the rebuild below is
                        // exactly what we need; nothing to forward.
                        warn!(error = %e, "M7 sinks reload: stream error");
                    }
                    Some(Ok(_)) => {
                        reload(&store, &registry, &file_sinks).await;
                    }
                }
            }
        }
    }
}

/// Re-read db sinks, merge with file sinks, swap the registry.
async fn reload(store: &Store, registry: &SinkRegistry, file_sinks: &[SinkConfig]) {
    let db_sink_json: Vec<String> = match store.alert_sinks_list().await {
        Ok(rows) => rows.into_iter().map(|r| r.config_json).collect(),
        Err(e) => {
            warn!(error = %e, "M7 sinks reload: failed to read alert_sinks; keeping previous registry");
            return;
        }
    };
    match nexus_sinks::build_effective_sinks(file_sinks, &db_sink_json) {
        Ok(sinks) => {
            let n = registry.replace(sinks);
            info!(n_sinks = n, "M7 sinks reload: registry rebuilt");
        }
        Err(e) => {
            warn!(error = %e, "M7 sinks reload: build_effective_sinks failed; keeping previous registry");
        }
    }
}
