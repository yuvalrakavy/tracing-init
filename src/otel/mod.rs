//! OpenTelemetry OTLP export support (feature-gated behind `otel`).

pub mod traces;
pub mod logs;

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;

/// Build an OTel Resource from service name and additional attributes.
pub fn build_resource(
    service_name: &str,
    extra_attrs: &[(String, String)],
) -> Resource {
    let mut attrs = vec![
        KeyValue::new("service.name", service_name.to_string()),
    ];
    for (key, value) in extra_attrs {
        attrs.push(KeyValue::new(key.clone(), value.clone()));
    }
    Resource::builder().with_attributes(attrs).build()
}
