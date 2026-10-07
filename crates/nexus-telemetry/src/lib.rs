//! Tracing + OpenTelemetry initialization.
//!
//! The pipeline opens a `frame.lifecycle` span per camera-frame and child
//! spans for `decode / gate / infer / track / rules`. This crate sets up
//! the subscriber so those spans are emitted (and, when configured,
//! exported via OTLP gRPC).
//!
//! ## Cloud trace shipping (Phase 1.14)
//!
//! Callers may pass a [`TraceUploaderHandle`] obtained from
//! [`nexus_cloud_client::trace_uploader::TraceUploader::channel`]. When
//! present, a [`TraceLayer`] is wired into the subscriber stack so every
//! engine span (subject to `EnvFilter`) is shipped to the edge-gateway
//! over the same mTLS identity the WSS tunnel uses. The consumer half
//! (the receiver) is spawned separately once cloud enrollment has been
//! read \u2014 see `nexus-engine`'s `cloud_tunnel::spawn_tunnel`.

#![forbid(unsafe_code)]

use anyhow::Result;
use nexus_cloud_client::trace_layer::{TraceLayer, TraceLayerConfig};
pub use nexus_cloud_client::trace_uploader::TraceUploaderHandle;
use nexus_config::TelemetryConfig;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{trace::Sampler, Resource};
use std::io::IsTerminal;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

pub struct TelemetryGuard {
    provider: Option<opentelemetry_sdk::trace::TracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(p) = self.provider.take() {
            // Best-effort flush; ignore errors during shutdown.
            let _ = p.shutdown();
        }
    }
}

/// True when stdout is connected to journald. systemd sets `JOURNAL_STREAM`
/// for a service whose stdout/stderr is a journal stream; it is absent for an
/// interactive run, where a `<N>` prefix would only be noise.
fn journal_prefix_enabled(journal_stream: Option<std::ffi::OsString>) -> bool {
    journal_stream.is_some_and(|v| !v.is_empty())
}

/// The sd-daemon(3) syslog priority prefix for a `tracing` level.
fn syslog_prefix(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "<3>",
        Level::WARN => "<4>",
        Level::INFO => "<6>",
        Level::DEBUG | Level::TRACE => "<7>",
    }
}

/// Wraps an event formatter, prepending the syslog priority prefix when
/// `enabled` so journald (`SyslogLevelPrefix=yes`) records the real level.
struct SyslogPrefix<F> {
    inner: F,
    enabled: bool,
}

impl<S, N, F> FormatEvent<S, N> for SyslogPrefix<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        if self.enabled {
            writer.write_str(syslog_prefix(event.metadata().level()))?;
        }
        self.inner.format_event(ctx, writer, event)
    }
}

/// Set up the tracing subscriber. Returns a guard that flushes OTLP on
/// drop.
///
/// `trace_handle` is the producer side of the
/// [`nexus_cloud_client::trace_uploader::TraceUploader`] channel; when
/// `Some`, a [`TraceLayer`] is added to the subscriber so every span
/// captured by the layer's `EnvFilter` is shipped to the edge-gateway.
/// When `None`, no cloud trace shipping is wired up (local-only mode).
pub fn init(
    cfg: &TelemetryConfig,
    trace_handle: Option<TraceUploaderHandle>,
) -> Result<TelemetryGuard> {
    let env_filter =
        EnvFilter::try_new(&cfg.log_level).unwrap_or_else(|_| EnvFilter::new("info,nexus=info"));

    let registry = tracing_subscriber::registry().with(env_filter);

    // ANSI only when stderr is a real terminal. Under systemd stderr is a
    // journald socket, and `tracing-subscriber` colourises unconditionally
    // by default — which writes raw SGR escapes into every journal record
    // (and into the formatted-field buffers the registry keeps alive per
    // open span). `journalctl` does not strip them, so `grep`/`jq` over the
    // journal matches against escape-laden text. Autodetect instead of
    // hardcoding: interactive `cargo run` keeps colour, the service does not.
    let ansi = std::io::stderr().is_terminal();

    // Under systemd, stdout is a journald stream that stamps every line
    // PRIORITY=6 unless the line starts with a `<N>` syslog prefix (#350).
    let journal = journal_prefix_enabled(std::env::var_os("JOURNAL_STREAM"));

    let fmt_layer = if cfg.json_logs {
        tracing_subscriber::fmt::layer()
            .json()
            .event_format(SyslogPrefix {
                inner: tracing_subscriber::fmt::format().json(),
                enabled: journal,
            })
            .boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .with_ansi(ansi)
            .event_format(SyslogPrefix {
                inner: tracing_subscriber::fmt::format()
                    .with_target(true)
                    .with_line_number(false)
                    .compact(),
                enabled: journal,
            })
            .boxed()
    };

    // Cloud trace shipping: `tracing_subscriber` implements `Layer` for
    // `Option<L>`, so `None` is a zero-cost no-op.
    //
    // Cost guardrail (Phase 6.17 follow-up): the per-frame pipeline
    // emits one span per stage per frame; at ~10 fps × 8 stages × N
    // cameras, that's millions of spans/day → ~17 GiB/30d ingest into
    // Application Insights. Drop the two highest-volume names entirely
    // (`frame.gate`, `frame.lifecycle` — inner-loop events; per-frame
    // timing belongs in metrics, not traces) and sample the remaining
    // `frame.*` spans at 1% by trace_id hash. Non-frame spans
    // (`camera.pipeline`, audit, RPC, etc.) ship at 100%.
    let trace_layer = trace_handle.map(|h| {
        let cfg = TraceLayerConfig {
            drop_names: ["frame.gate", "frame.lifecycle"]
                .into_iter()
                .map(String::from)
                .collect(),
            prefix_sample: vec![("frame.".to_string(), 0.01)],
        };
        TraceLayer::with_config(h, cfg)
    });

    let mut guard = TelemetryGuard { provider: None };

    if let Some(otlp) = &cfg.otlp {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&otlp.endpoint)
            .build()?;

        let resource = Resource::new(vec![
            opentelemetry::KeyValue::new(
                "service.name",
                otlp.service_name
                    .clone()
                    .unwrap_or_else(|| "nexus-engine".into()),
            ),
            opentelemetry::KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        ]);

        let sampler = if otlp.sample_ratio >= 1.0 {
            Sampler::AlwaysOn
        } else if otlp.sample_ratio <= 0.0 {
            Sampler::AlwaysOff
        } else {
            Sampler::TraceIdRatioBased(otlp.sample_ratio)
        };

        let provider = opentelemetry_sdk::trace::TracerProvider::builder()
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_sampler(sampler)
            .with_resource(resource)
            .build();

        let tracer = provider.tracer("nexus-engine");
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

        registry
            .with(fmt_layer)
            .with(otel_layer)
            .with(trace_layer)
            .try_init()?;
        guard.provider = Some(provider);
    } else {
        registry.with(fmt_layer).with(trace_layer).try_init()?;
    }

    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syslog_prefix_maps_every_level() {
        assert_eq!(syslog_prefix(&Level::ERROR), "<3>");
        assert_eq!(syslog_prefix(&Level::WARN), "<4>");
        assert_eq!(syslog_prefix(&Level::INFO), "<6>");
        assert_eq!(syslog_prefix(&Level::DEBUG), "<7>");
        assert_eq!(syslog_prefix(&Level::TRACE), "<7>");
    }

    #[test]
    fn prefix_only_when_journal_stream_is_set() {
        assert!(!journal_prefix_enabled(None));
        assert!(!journal_prefix_enabled(Some("".into())));
        assert!(journal_prefix_enabled(Some("8:12345".into())));
    }

    #[test]
    fn formatter_prepends_prefix_only_when_enabled() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Buf {
                self.clone()
            }
        }

        for (enabled, want) in [(true, "<4>"), (false, "")] {
            let buf = Buf::default();
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(buf.clone())
                .event_format(SyslogPrefix {
                    inner: tracing_subscriber::fmt::format().compact(),
                    enabled,
                });
            let sub = tracing_subscriber::registry().with(layer);
            tracing::subscriber::with_default(sub, || tracing::warn!("boom"));
            let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
            assert!(out.contains("WARN"), "{out}");
            assert_eq!(out.starts_with('<'), enabled, "{out}");
            assert!(out.starts_with(want), "{out}");
        }
    }
}
