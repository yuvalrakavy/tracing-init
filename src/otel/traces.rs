//! OTel trace exporter layer construction.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::Layer;

use super::OtelBoxedLayer;

/// Create a TracerProvider with OTLP exporter.
///
/// Returns the provider (held in TracingGuard for shutdown)
/// and the tracing-opentelemetry layer as a boxed trait object.
pub fn create_trace_layer(
    endpoint: &str,
    #[allow(unused_variables)]
    transport: &str,
    resource: Resource,
) -> Result<(SdkTracerProvider, OtelBoxedLayer), Box<dyn std::error::Error>>
{
    let exporter = match transport {
        #[cfg(feature = "otel-grpc")]
        "grpc" => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()?,
        _ => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(format!("{endpoint}/v1/traces"))
            .build()?,
    };

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("tracing-init");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);

    Ok((provider, layer.boxed()))
}
