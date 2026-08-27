//! OTel trace exporter layer construction.

use std::sync::Arc;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use tracing_subscriber::Layer;

use super::circuit_breaker::{CircuitBreakerSpanExporter, CircuitState};
use super::OtelBoxedLayer;

/// Create a TracerProvider with OTLP exporter wrapped in a circuit breaker.
///
/// Returns the provider (held in TracingGuard for shutdown)
/// and the tracing-opentelemetry layer as a boxed trait object.
///
/// `span_queue_size` is `Some` ONLY when the depth came from TOML. `None` leaves the batch
/// processor entirely alone, so `BatchConfig::default()` runs `init_from_env_vars()` and
/// `OTEL_BSP_MAX_QUEUE_SIZE` applies exactly as it does in any other OTel program. That is
/// the mechanism, not an optimisation: the SDK documents that programmatic configuration
/// OVERRIDES the environment variable, so the only way to let env win is to not configure.
pub fn create_trace_layer(
    endpoint: &str,
    #[allow(unused_variables)] transport: &str,
    resource: Resource,
    circuit_state: Arc<CircuitState>,
    span_queue_size: Option<usize>,
) -> Result<(SdkTracerProvider, OtelBoxedLayer), Box<dyn std::error::Error>> {
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

    let wrapped = CircuitBreakerSpanExporter::new(exporter, circuit_state);

    let provider = match span_queue_size {
        Some(depth) => {
            let batch = BatchSpanProcessor::builder(wrapped)
                .with_batch_config(
                    BatchConfigBuilder::default()
                        .with_max_queue_size(depth)
                        .build(),
                )
                .build();
            SdkTracerProvider::builder()
                .with_span_processor(batch)
                .with_resource(resource)
                .build()
        }
        None => SdkTracerProvider::builder()
            .with_batch_exporter(wrapped)
            .with_resource(resource)
            .build(),
    };

    let tracer = provider.tracer("tracing-init");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);

    Ok((provider, layer.boxed()))
}
