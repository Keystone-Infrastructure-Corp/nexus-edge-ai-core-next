//! # trace_layer
//!
//! Phase 1.14b — `tracing_subscriber::Layer` that captures every
//! `tracing::span!` open/close in the engine and pushes the resulting
//! [`Span`] into a [`TraceUploaderHandle`]. The uploader batches and
//! ships to the edge-gateway over the same mTLS identity the WSS
//! tunnel uses; the gateway forwards to the api-gateway, which
//! translates each span into an Application Insights
//! `RemoteDependencyData` envelope.
//!
//! ## What we capture
//!
//! * `span_id` — 8 fresh random bytes (16 hex chars) per opened span.
//! * `trace_id` — inherited from parent if present, otherwise 16 fresh
//!   random bytes (32 hex chars). Spans opened during the processing
//!   of an inbound request that carried a W3C `traceparent` should
//!   propagate that trace_id via a parent span (e.g. a `tracing::span`
//!   opened by the HTTP handler with the incoming trace_id stamped on
//!   it — wiring not covered by this layer).
//! * `parent_span_id` — the immediate parent's `span_id`, omitted for
//!   root spans.
//! * `name` — the span's metadata name (`tracing::span!(name = ...)`).
//! * Attributes — every field set via `tracing::span!(... key = value)`
//!   or `span.record("key", &value)`, captured as `String`.
//! * `start_unix_ns` — wall-clock time at `on_new_span`.
//! * `end_unix_ns` — wall-clock time at `on_close`.
//! * `status` — `Error` if any field named `error` exists, else `Ok`.
//!
//! ## What we don't capture
//!
//! Events (`tracing::event!`) and structured logs are intentionally
//! ignored — App Insights' RemoteDependencyData has no equivalent
//! surface, and the engine's fmt-layer already ships them to stdout.
//!
//! ## Filtering
//!
//! Compose the layer with an `EnvFilter` to scope which spans get
//! shipped. Default suggestion: `info,nexus_engine=debug` — INFO-level
//! and above for everything, debug for nexus-engine.

use std::collections::{HashMap, HashSet};
use std::fmt;

use tracing::field::{Field, Visit};
use tracing::span::Attributes;
use tracing::{Id, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::trace_uploader::{now_unix_ns, Span, SpanKind, SpanStatus, TraceUploaderHandle};

/// Cost guardrail for the cloud trace pipeline.
///
/// Without filtering, every per-frame span the engine emits (the
/// `frame.lifecycle` root plus `frame.gate / infer / track / …` children)
/// is shipped to `traces-svc` and ingested into Application Insights.
/// At ~10 fps × 8 stages per frame × multiple cameras that's millions of
/// `AppDependencies` rows per day; a single dev workspace burned ~17 GiB
/// in 30 days that way.
///
/// Two knobs:
/// * [`drop_names`](TraceLayerConfig::drop_names) — span names that are
///   never shipped. The local `fmt` layer still prints them; only the
///   cloud export drops. Children of a dropped span lose the trace_id
///   inheritance chain (their nearest ancestor with `SpanData` is now
///   higher up, or none), so they become roots of their own trace and
///   are subject to any matching `prefix_sample` rule independently.
/// * [`prefix_sample`](TraceLayerConfig::prefix_sample) — for any span
///   whose name starts with `prefix`, ship only `ratio` (0.0..=1.0) of
///   those spans. The decision is keyed on `hash(trace_id) mod 10_000`,
///   so all spans in the same trace are kept or dropped together; first
///   matching prefix wins.
///
/// Spans matching neither rule are shipped at 100%.
#[derive(Debug, Clone, Default)]
pub struct TraceLayerConfig {
    pub drop_names: HashSet<String>,
    pub prefix_sample: Vec<(String, f64)>,
}

impl TraceLayerConfig {
    /// Returns `true` if a span with this name should be exported.
    fn should_ship(&self, name: &str, trace_id: &str) -> bool {
        if self.drop_names.contains(name) {
            return false;
        }
        for (prefix, ratio) in &self.prefix_sample {
            if name.starts_with(prefix.as_str()) {
                if *ratio <= 0.0 {
                    return false;
                }
                if *ratio >= 1.0 {
                    return true;
                }
                // Hash first 8 hex chars of trace_id (32 bits) → mod 10_000.
                // Keeps all spans sharing a trace_id together (when the
                // chain is intact) and is deterministic per trace.
                let bucket = trace_id_bucket(trace_id);
                let cutoff = (ratio * 10_000.0) as u32;
                return bucket < cutoff;
            }
        }
        true
    }
}

/// Parse the leading 8 hex chars of `trace_id` as a `u32` and reduce
/// to `[0, 10_000)`. Returns `0` (i.e. keep) on malformed input — the
/// random_trace_id() generator always yields 32 hex chars, so this only
/// trips on test fixtures that pass `""`.
fn trace_id_bucket(trace_id: &str) -> u32 {
    if trace_id.len() < 8 {
        return 0;
    }
    u32::from_str_radix(&trace_id[..8], 16)
        .map(|v| v % 10_000)
        .unwrap_or(0)
}

/// Per-span state cached in the subscriber's per-span extensions
/// between `on_new_span` and `on_close`.
struct SpanData {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    start_unix_ns: u64,
    attributes: HashMap<String, String>,
    /// Set to `Error` if any field was named `error`.
    status: SpanStatus,
}

/// Visitor that copies every span field into a [`HashMap<String, String>`].
struct FieldVisitor<'a> {
    attrs: &'a mut HashMap<String, String>,
    status: &'a mut SpanStatus,
}

impl<'a> Visit for FieldVisitor<'a> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let name = field.name();
        // tracing wraps str values as `Debug`-only; strip outer quotes.
        let formatted = format!("{value:?}");
        let stripped = formatted
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(&formatted);
        self.attrs.insert(name.to_string(), stripped.to_string());
        if name == "error" {
            *self.status = SpanStatus::Error;
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.attrs
            .insert(field.name().to_string(), value.to_string());
        if field.name() == "error" {
            *self.status = SpanStatus::Error;
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.attrs
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.attrs
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.attrs
            .insert(field.name().to_string(), value.to_string());
    }
}

/// `tracing_subscriber::Layer` that ships span open/close to a
/// [`TraceUploaderHandle`]. Cheap to clone (handle is an `Arc`).
#[derive(Clone)]
pub struct TraceLayer {
    handle: TraceUploaderHandle,
    config: std::sync::Arc<TraceLayerConfig>,
}

impl TraceLayer {
    /// Wrap an uploader handle into a tracing layer with no filtering
    /// (every span shipped). Equivalent to
    /// `with_config(handle, TraceLayerConfig::default())`.
    #[must_use]
    pub fn new(handle: TraceUploaderHandle) -> Self {
        Self::with_config(handle, TraceLayerConfig::default())
    }

    /// Wrap an uploader handle into a tracing layer that drops /
    /// samples spans per [`TraceLayerConfig`]. Used by the engine's
    /// `nexus-telemetry::init` to keep AI ingest costs bounded.
    #[must_use]
    pub fn with_config(handle: TraceUploaderHandle, config: TraceLayerConfig) -> Self {
        Self {
            handle,
            config: std::sync::Arc::new(config),
        }
    }
}

impl<S> Layer<S> for TraceLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span_ref) = ctx.span(id) else {
            return;
        };
        // Inherit trace_id + parent_span_id from the nearest ancestor
        // that we tagged. If none, mint a fresh trace_id (root).
        let (trace_id, parent_span_id) = span_ref
            .scope()
            .skip(1) // skip self
            .find_map(|s| {
                let exts = s.extensions();
                let parent: Option<&SpanData> = exts.get();
                parent.map(|p| (p.trace_id.clone(), Some(p.span_id.clone())))
            })
            .unwrap_or_else(|| (random_trace_id(), None));

        // Cost guardrail: drop or sample by name before allocating
        // SpanData. A dropped span has no SpanData, so on_close won't
        // push it, and its children's `scope().find_map(...)` above
        // skips past it — they re-root with a fresh trace_id and are
        // independently subject to the same rules.
        let name = span_ref.name();
        if !self.config.should_ship(name, &trace_id) {
            return;
        }

        let mut data = SpanData {
            trace_id,
            span_id: random_span_id(),
            parent_span_id,
            start_unix_ns: now_unix_ns(),
            attributes: HashMap::new(),
            status: SpanStatus::Ok,
        };
        // Capture initial attributes set on `span!(...)`.
        let mut visitor = FieldVisitor {
            attrs: &mut data.attributes,
            status: &mut data.status,
        };
        attrs.record(&mut visitor);

        span_ref.extensions_mut().insert(data);
    }

    fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, ctx: Context<'_, S>) {
        // Capture late-set fields from `span.record("key", &value)`.
        let Some(span_ref) = ctx.span(id) else {
            return;
        };
        let mut exts = span_ref.extensions_mut();
        let Some(data) = exts.get_mut::<SpanData>() else {
            return;
        };
        let mut visitor = FieldVisitor {
            attrs: &mut data.attributes,
            status: &mut data.status,
        };
        values.record(&mut visitor);
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span_ref) = ctx.span(&id) else {
            return;
        };
        let name = span_ref.name().to_string();
        let mut exts = span_ref.extensions_mut();
        let Some(data) = exts.remove::<SpanData>() else {
            return;
        };
        let span = Span {
            trace_id: data.trace_id,
            span_id: data.span_id,
            parent_span_id: data.parent_span_id,
            name,
            kind: SpanKind::Internal,
            start_unix_ns: data.start_unix_ns,
            end_unix_ns: now_unix_ns(),
            status: data.status,
            attributes: data.attributes,
        };
        // Best-effort push. Drop silently on `QueueFull` / `Stopped`;
        // the uploader's `dropped_count()` records the loss.
        let _ = self.handle.push(span);
    }
}

/// 16 random bytes → 32 hex chars. On the (impossible) RNG failure,
/// uses a zero trace_id — better than panicking inside a tracing hook.
fn random_trace_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        bytes = [0; 16];
    }
    hex_encode(&bytes)
}

/// 8 random bytes → 16 hex chars.
fn random_span_id() -> String {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        bytes = [0; 8];
    }
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::trace_uploader::{BatchTransport, TraceBatch, TraceUploader, TraceUploaderConfig};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Default, Clone)]
    struct RecordingTransport {
        batches: Arc<TokioMutex<Vec<TraceBatch>>>,
    }

    #[async_trait::async_trait]
    impl BatchTransport for RecordingTransport {
        async fn send_batch(&self, batch: TraceBatch) -> Result<(), String> {
            self.batches.lock().await.push(batch);
            Ok(())
        }
    }

    #[test]
    fn random_trace_id_is_32_hex_chars() {
        let t = random_trace_id();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn random_span_id_is_16_hex_chars() {
        let s = random_span_id();
        assert_eq!(s.len(), 16);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn random_span_ids_are_unique_across_many_draws() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(random_span_id()), "duplicate span_id");
        }
    }

    #[test]
    fn hex_encode_round_trips_known_bytes() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0xab, 0xcd]), "00ffabcd");
    }

    #[tokio::test]
    async fn layer_captures_a_simple_span() {
        let transport = Arc::new(RecordingTransport::default());
        let cfg = TraceUploaderConfig {
            endpoint_url: "https://test/".into(),
            core_id: uuid::Uuid::nil(),
            batch_size: 1,
            flush_interval: Duration::from_secs(60),
            queue_capacity: 16,
        };
        let (handle, _join) =
            TraceUploader::spawn(cfg, transport.clone() as Arc<dyn BatchTransport>);
        let layer = TraceLayer::new(handle);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let _enter = tracing::info_span!("test.span", key = "value").entered();
            // span closes on drop at end of closure.
        });
        // Give the uploader a chance to flush.
        for _ in 0..20 {
            if !transport.batches.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let batches = transport.batches.lock().await;
        assert_eq!(batches.len(), 1);
        let span = &batches[0].spans[0];
        assert_eq!(span.name, "test.span");
        assert_eq!(
            span.attributes.get("key").map(String::as_str),
            Some("value")
        );
        assert_eq!(span.status, SpanStatus::Ok);
        assert!(span.parent_span_id.is_none());
        assert_eq!(span.trace_id.len(), 32);
        assert_eq!(span.span_id.len(), 16);
        assert!(span.end_unix_ns >= span.start_unix_ns);
    }

    #[tokio::test]
    async fn child_span_inherits_trace_id_and_parent_id() {
        let transport = Arc::new(RecordingTransport::default());
        let cfg = TraceUploaderConfig {
            endpoint_url: "https://test/".into(),
            core_id: uuid::Uuid::nil(),
            batch_size: 2,
            flush_interval: Duration::from_secs(60),
            queue_capacity: 16,
        };
        let (handle, _join) =
            TraceUploader::spawn(cfg, transport.clone() as Arc<dyn BatchTransport>);
        let layer = TraceLayer::new(handle);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let parent = tracing::info_span!("parent");
            let _e = parent.enter();
            let _child = tracing::info_span!("child").entered();
        });
        for _ in 0..20 {
            if transport
                .batches
                .lock()
                .await
                .iter()
                .map(|b| b.spans.len())
                .sum::<usize>()
                >= 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let batches = transport.batches.lock().await;
        let all: Vec<_> = batches.iter().flat_map(|b| b.spans.iter()).collect();
        assert_eq!(all.len(), 2, "expected exactly two spans");
        let child = all.iter().find(|s| s.name == "child").expect("child span");
        let parent = all
            .iter()
            .find(|s| s.name == "parent")
            .expect("parent span");
        assert_eq!(child.trace_id, parent.trace_id, "trace_id must propagate");
        assert_eq!(
            child.parent_span_id.as_deref(),
            Some(parent.span_id.as_str()),
            "parent_span_id must point to parent's span_id"
        );
        assert!(parent.parent_span_id.is_none(), "root span has no parent");
    }

    #[tokio::test]
    async fn error_field_marks_span_status_as_error() {
        let transport = Arc::new(RecordingTransport::default());
        let cfg = TraceUploaderConfig {
            endpoint_url: "https://test/".into(),
            core_id: uuid::Uuid::nil(),
            batch_size: 1,
            flush_interval: Duration::from_secs(60),
            queue_capacity: 16,
        };
        let (handle, _join) =
            TraceUploader::spawn(cfg, transport.clone() as Arc<dyn BatchTransport>);
        let layer = TraceLayer::new(handle);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let _s = tracing::info_span!("with.error", error = "boom").entered();
        });
        for _ in 0..20 {
            if !transport.batches.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let batches = transport.batches.lock().await;
        let span = &batches[0].spans[0];
        assert_eq!(span.status, SpanStatus::Error);
    }

    #[test]
    fn drop_names_filter_blocks_listed_spans() {
        let cfg = TraceLayerConfig {
            drop_names: ["frame.gate", "frame.lifecycle"]
                .into_iter()
                .map(String::from)
                .collect(),
            prefix_sample: vec![],
        };
        // Any trace_id; drop_names is name-only.
        assert!(!cfg.should_ship("frame.gate", "00000000ffffffff"));
        assert!(!cfg.should_ship("frame.lifecycle", "00000000ffffffff"));
        assert!(cfg.should_ship("frame.infer", "00000000ffffffff"));
        assert!(cfg.should_ship("camera.pipeline", "00000000ffffffff"));
    }

    #[test]
    fn prefix_sample_keeps_approximately_ratio_fraction() {
        let cfg = TraceLayerConfig {
            drop_names: HashSet::new(),
            prefix_sample: vec![("frame.".to_string(), 0.01)],
        };
        // Sweep all 10_000 buckets deterministically — exactly 1% should
        // be kept (buckets [0, 100)) because should_ship uses the same
        // mod-10_000 math.
        let kept = (0u32..10_000)
            .filter(|i| {
                let trace_id = format!("{i:08x}{:024x}", 0u128);
                cfg.should_ship("frame.infer", &trace_id)
            })
            .count();
        assert_eq!(kept, 100, "1% of 10_000 buckets should pass");
    }

    #[test]
    fn prefix_sample_ratio_one_keeps_all_and_zero_drops_all() {
        let keep_all = TraceLayerConfig {
            drop_names: HashSet::new(),
            prefix_sample: vec![("frame.".to_string(), 1.0)],
        };
        let drop_all = TraceLayerConfig {
            drop_names: HashSet::new(),
            prefix_sample: vec![("frame.".to_string(), 0.0)],
        };
        for i in 0u32..1000 {
            let trace_id = format!("{i:08x}{:024x}", 0u128);
            assert!(keep_all.should_ship("frame.infer", &trace_id));
            assert!(!drop_all.should_ship("frame.infer", &trace_id));
        }
    }

    #[test]
    fn unmatched_spans_are_always_shipped() {
        let cfg = TraceLayerConfig {
            drop_names: HashSet::new(),
            prefix_sample: vec![("frame.".to_string(), 0.0)], // would drop frame.*
        };
        // Not a frame.* — no prefix matches, so ship.
        assert!(cfg.should_ship("camera.pipeline", "deadbeef0000000000000000"));
        assert!(cfg.should_ship("audit.event", "0000beef0000000000000000"));
    }

    #[tokio::test]
    async fn with_config_drops_listed_span_end_to_end() {
        let transport = Arc::new(RecordingTransport::default());
        let cfg = TraceUploaderConfig {
            endpoint_url: "https://test/".into(),
            core_id: uuid::Uuid::nil(),
            batch_size: 1,
            flush_interval: Duration::from_millis(50),
            queue_capacity: 16,
        };
        let (handle, _join) =
            TraceUploader::spawn(cfg, transport.clone() as Arc<dyn BatchTransport>);
        let layer_cfg = TraceLayerConfig {
            drop_names: ["frame.gate"].into_iter().map(String::from).collect(),
            prefix_sample: vec![],
        };
        let layer = TraceLayer::with_config(handle, layer_cfg);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let _dropped = tracing::info_span!("frame.gate").entered();
        });
        // Give the uploader at least one flush window (50 ms) to prove
        // nothing showed up.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let batches = transport.batches.lock().await;
        assert!(
            batches.iter().all(|b| b.spans.is_empty()),
            "frame.gate must not be exported, got {batches:?}"
        );
    }
}
