//! OpenTelemetry OTLP tracing + `tracing_subscriber` integration.
//!
//! Enable by setting `OTEL_EXPORTER_OTLP_ENDPOINT` (e.g. `http://localhost:4317` for Jaeger OTLP gRPC).

use anyhow::{Context, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
use opentelemetry_sdk::Resource;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Initialize global `tracing` subscriber. Call once at process start.
///
/// If `OTEL_EXPORTER_OTLP_ENDPOINT` is unset or empty, only stdout logging is configured (no OTLP).
pub fn init_tracing() -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive(
            "polymarket_llm_bot=info"
                .parse()
                .expect("static env filter directive"),
        );

    let log_json = std::env::var("LOG_JSON")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or_default();
    let otel_on = !endpoint.is_empty();

    if otel_on {
        let service_name = std::env::var("OTEL_SERVICE_NAME")
            .unwrap_or_else(|_| "polymarket-llm-bot".to_string());

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .context("failed to build OTLP span exporter")?;

        let provider = TracerProvider::builder()
            .with_resource(Resource::new(vec![KeyValue::new("service.name", service_name)]))
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .build();

        // Use the SDK tracer (not `global::tracer`) so `tracing_opentelemetry` gets `PreSampledTracer`.
        let tracer = provider.tracer("polymarket-llm-bot");
        opentelemetry::global::set_tracer_provider(provider);
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

        if log_json {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(otel_layer)
                .with(tracing_subscriber::fmt::layer().json())
                .init();
        } else {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(otel_layer)
                .with(tracing_subscriber::fmt::layer())
                .init();
        }
    } else if log_json {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            .init();
    }

    Ok(())
}

/// Spawn a background task that shuts down the OTel tracer on Ctrl+C (flushes pending spans).
pub fn spawn_otel_shutdown_on_ctrl_c() {
    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            opentelemetry::global::shutdown_tracer_provider();
        });
    }
}
