//! Tracing + OpenTelemetry initialization.
//!
//! The pipeline opens a `frame.lifecycle` span per camera-frame and child
//! spans for `decode / gate / infer / track / rules`. This crate sets up
//! the subscriber so those spans are emitted (and, when configured,
//! exported via OTLP gRPC).

#![forbid(unsafe_code)]

use anyhow::Result;
use nexus_config::TelemetryConfig;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{trace::Sampler, Resource};
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

/// Set up the tracing subscriber. Returns a guard that flushes OTLP on drop.
pub fn init(cfg: &TelemetryConfig) -> Result<TelemetryGuard> {
    let env_filter = EnvFilter::try_new(&cfg.log_level)
        .unwrap_or_else(|_| EnvFilter::new("info,nexus=debug"));

    let registry = tracing_subscriber::registry().with(env_filter);

    let fmt_layer = if cfg.json_logs {
        tracing_subscriber::fmt::layer().json().boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_line_number(false)
            .compact()
            .boxed()
    };

    let mut guard = TelemetryGuard { provider: None };

    if let Some(otlp) = &cfg.otlp {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&otlp.endpoint)
            .build()?;

        let resource = Resource::new(vec![
            opentelemetry::KeyValue::new(
                "service.name",
                otlp.service_name.clone().unwrap_or_else(|| "nexus-engine".into()),
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

        registry.with(fmt_layer).with(otel_layer).try_init()?;
        guard.provider = Some(provider);
    } else {
        registry.with(fmt_layer).try_init()?;
    }

    Ok(guard)
}
