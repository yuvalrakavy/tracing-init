//! OTel log exporter layer construction.

use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use tracing_subscriber::Layer;

use super::OtelBoxedLayer;

/// Create a LoggerProvider with OTLP exporter.
///
/// Returns the provider (held in TracingGuard for shutdown)
/// and the log bridge layer as a boxed trait object.
pub fn create_log_layer(
    endpoint: &str,
    #[allow(unused_variables)]
    transport: &str,
    resource: Resource,
) -> Result<(SdkLoggerProvider, OtelBoxedLayer), Box<dyn std::error::Error>>
{
    let exporter = match transport {
        #[cfg(feature = "otel-grpc")]
        "grpc" => opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()?,
        _ => opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_endpoint(format!("{endpoint}/v1/logs"))
            .build()?,
    };

    let provider = SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let layer = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider);

    Ok((provider, layer.boxed()))
}
