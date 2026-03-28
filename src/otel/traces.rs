//! OTel trace exporter layer construction.

use std::sync::Arc;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::Layer;

use super::OtelBoxedLayer;
use super::circuit_breaker::{CircuitBreakerSpanExporter, CircuitState};

/// Create a TracerProvider with OTLP exporter wrapped in a circuit breaker.
///
/// Returns the provider (held in TracingGuard for shutdown)
/// and the tracing-opentelemetry layer as a boxed trait object.
pub fn create_trace_layer(
    endpoint: &str,
    #[allow(unused_variables)]
    transport: &str,
    resource: Resource,
    circuit_state: Arc<CircuitState>,
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

    let wrapped = CircuitBreakerSpanExporter::new(Box::new(exporter), circuit_state);

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(wrapped)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("tracing-init");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);

    Ok((provider, layer.boxed()))
}
