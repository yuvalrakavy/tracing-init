//! Support for the logmon_test attribute. Each nextest attempt owns one process.
use crate::gelf::GelfLayer;
use opentelemetry::{
    trace::{SpanId, TraceId, TracerProvider},
    KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    error::OTelSdkResult,
    trace::{IdGenerator, RandomIdGenerator, SdkTracerProvider, SpanData, SpanExporter},
    Resource,
};
use std::{future::Future, time::Duration};
use tracing::{Instrument, Span};
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Layer};

#[derive(Debug)]
struct AttemptIds {
    trace: TraceId,
    span_generator: RandomIdGenerator,
}

impl IdGenerator for AttemptIds {
    fn new_trace_id(&self) -> TraceId {
        self.trace
    }
    fn new_span_id(&self) -> SpanId {
        self.span_generator.new_span_id()
    }
}

#[derive(Debug)]
struct AttemptExporter {
    inner: opentelemetry_otlp::SpanExporter,
    attribute: KeyValue,
}

impl SpanExporter for AttemptExporter {
    async fn export(&self, mut batch: Vec<SpanData>) -> OTelSdkResult {
        for span in &mut batch {
            span.attributes.retain(|v| v.key != self.attribute.key);
            span.attributes.push(self.attribute.clone());
        }
        self.inner.export(batch).await
    }
    fn shutdown(&mut self) -> OTelSdkResult {
        self.inner.shutdown()
    }
    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

pub struct Attempt {
    pub span: Span,
    provider: SdkTracerProvider,
}
impl Drop for Attempt {
    fn drop(&mut self) {
        // The root must close before batched exports are flushed even on panic.
        drop(std::mem::replace(&mut self.span, Span::none()));
        let _ = self.provider.shutdown_with_timeout(Duration::from_secs(1));
    }
}

pub fn begin(name: &'static str) -> Option<Attempt> {
    let id = std::env::var("LOGMON_TEST_TRACE_ID").ok()?;
    let result = install(name, &id);
    if let Err(e) = &result {
        eprintln!("logmon test instrumentation unavailable: {e}");
    }
    result.ok()
}

fn install(name: &'static str, id: &str) -> Result<Attempt, Box<dyn std::error::Error>> {
    let trace = TraceId::from_hex(id)?;
    if id.len() != 32 || trace == TraceId::INVALID {
        return Err("LOGMON_TEST_TRACE_ID must be 32 nonzero hexadecimal digits".into());
    }
    let gelf_addr =
        std::env::var("LOGMON_TEST_GELF_ADDR").unwrap_or_else(|_| "127.0.0.1:12201".into());
    let endpoint = std::env::var("LOGMON_TEST_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:4318".into());
    let inner = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{}/v1/traces", endpoint.trim_end_matches('/')))
        .build()?;
    let exporter = AttemptExporter {
        inner,
        attribute: KeyValue::new("test_attempt_id", id.to_owned()),
    };
    let mut gelf = GelfLayer::new(&gelf_addr, vec![], Some("rust-test".into()))?;
    gelf.test_attempt_id = Some(id.to_owned());
    let provider = SdkTracerProvider::builder()
        .with_resource(crate::otel::build_resource("rust-test", &[]))
        .with_id_generator(AttemptIds {
            trace,
            span_generator: RandomIdGenerator::default(),
        })
        .with_batch_exporter(exporter)
        .build();
    let tracer = provider.tracer("rust-test");
    // A normal INFO threshold prevents exporter-internal debug logs from recursively producing telemetry.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry()
            .with(gelf.with_filter(filter.clone()))
            .with(
                tracing_opentelemetry::layer()
                    .with_tracer(tracer)
                    .with_filter(filter),
            ),
    )?;
    let _ = tracing_log::LogTracer::init();
    Ok(Attempt {
        span: tracing::info_span!("rust.test", test.name = name),
        provider,
    })
}

pub async fn instrument<F: Future>(future: F, attempt: &Option<Attempt>) -> F::Output {
    match attempt {
        Some(attempt) => future.instrument(attempt.span.clone()).await,
        None => future.await,
    }
}
