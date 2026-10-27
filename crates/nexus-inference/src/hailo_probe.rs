//! Immutable device/HEF diagnostics follow the lifetime of the owning worker.
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::{Arc, LazyLock, Weak};

pub(crate) type WorkerProbe = Mutex<Option<Value>>;
static WORKERS: LazyLock<Mutex<Vec<Weak<WorkerProbe>>>> = LazyLock::new(Mutex::default);

pub(crate) fn register_worker() -> Arc<WorkerProbe> {
    let probe = Arc::new(Mutex::new(None));
    let mut workers = WORKERS.lock();
    workers.retain(|worker| worker.strong_count() > 0);
    workers.push(Arc::downgrade(&probe));
    probe
}

pub fn snapshot() -> Result<Option<Value>, crate::InferenceError> {
    #[cfg(all(feature = "ep-hailo", feature = "ort"))]
    if let Some(probe) = crate::hailo_yolo::probe_snapshot()? {
        return Ok(Some(probe));
    }
    let mut workers = WORKERS.lock();
    workers.retain(|worker| worker.strong_count() > 0);
    Ok(workers
        .iter()
        .filter_map(Weak::upgrade)
        .find_map(|worker| worker.lock().clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_probe_does_not_keep_a_retired_session_alive() {
        let probe = register_worker();
        let weak = Arc::downgrade(&probe);
        *probe.lock() = Some(serde_json::json!({"devices": [{"serial": "test"}]}));
        assert!(weak.upgrade().unwrap().lock().is_some());
        drop(probe);
        assert!(weak.upgrade().is_none());
    }
}
