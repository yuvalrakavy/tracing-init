//! Lightweight GELF (Graylog Extended Log Format) layer for [`tracing-subscriber`].
//!
//! Sends JSON-encoded [GELF 1.1](https://go2docs.graylog.org/current/getting_in_log_data/gelf.html)
//! messages over a `std::net::UdpSocket`. The implementation is deliberately simple:
//!
//! - **No async runtime required** -- uses blocking UDP sends.
//! - **No background threads or channels** -- each event is serialized and sent inline.
//! - **Best-effort delivery** -- send failures are silently ignored (standard for UDP logging).
//!
//! # GELF Field Mapping
//!
//! | Tracing concept | GELF field |
//! |-----------------|------------|
//! | Event message | `short_message` |
//! | Level (ERROR/WARN/INFO/DEBUG/TRACE) | `level` (syslog numeric: 3/4/6/7/7) |
//! | Level name | `_level` (string, distinguishes DEBUG from TRACE) |
//! | Source file | `_file` |
//! | Source line | `_line` |
//! | Target (module path) | `_target` |
//! | Service name | `_service` |
//! | Current span name | `_span_name` |
//! | Span fields | `_span_<field>` |
//! | OTel trace ID (otel feature) | `_trace_id` |
//! | OTel span ID (otel feature) | `_span_id` |
//! | Other fields | `_<field_name>` |

use serde_json::{json, Map, Value};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use tracing::field::{Field, Visit};
use tracing::Level;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// A [`tracing_subscriber::Layer`] that sends events as GELF messages over UDP.
///
/// Create with [`GelfLayer::new`] and register it with a
/// [`tracing_subscriber::Registry`].
pub struct GelfLayer {
    socket: UdpSocket,
    addr: SocketAddr,
    base_fields: Map<String, Value>,
    service_name: Option<String>,
}

impl GelfLayer {
    /// Create a new GELF layer that sends to the given `host:port` address.
    ///
    /// The `additional_fields` are included in every GELF message as `_<key>` fields.
    /// The local hostname is automatically resolved and included as the GELF `host` field.
    ///
    /// # Errors
    ///
    /// Returns an error if the address cannot be resolved or the UDP socket cannot be bound.
    pub fn new(
        addr: &str,
        additional_fields: Vec<(&str, String)>,
        service_name: Option<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let resolved = addr
            .to_socket_addrs()?
            .next()
            .ok_or("could not resolve GELF server address")?;

        let bind_addr = if resolved.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = UdpSocket::bind(bind_addr)?;

        let hostname = hostname::get()
            .unwrap_or_else(|_| "unknown".into())
            .into_string()
            .unwrap_or_else(|_| "unknown".into());

        let mut base_fields = Map::new();
        base_fields.insert("version".into(), json!("1.1"));
        base_fields.insert("host".into(), json!(hostname));
        for (k, v) in additional_fields {
            base_fields.insert(format!("_{k}"), json!(v));
        }

        Ok(GelfLayer {
            socket,
            addr: resolved,
            base_fields,
            service_name,
        })
    }
}

/// Add service name to GELF fields if set.
pub(crate) fn add_service_field(fields: &mut Map<String, Value>, service_name: Option<&str>) {
    if let Some(service) = service_name {
        fields.insert("_service".into(), json!(service));
    }
}

/// Add tracing metadata to GELF fields.
pub(crate) fn add_metadata_fields(
    fields: &mut Map<String, Value>,
    target: Option<&str>,
    file: Option<&str>,
    line: Option<u32>,
) {
    if let Some(target) = target {
        fields.insert("_target".into(), json!(target));
    }
    if let Some(file) = file {
        fields.insert("_file".into(), json!(file));
    }
    if let Some(line) = line {
        fields.insert("_line".into(), json!(line));
    }
}

/// Stores span field key-value pairs for later inclusion in GELF messages.
#[derive(Debug)]
struct SpanFields {
    fields: Vec<(String, Value)>,
}

/// Visitor that collects span attributes into a `Vec` of key-value pairs.
struct SpanFieldVisitor<'a> {
    fields: &'a mut Vec<(String, Value)>,
}

impl<'a> Visit for SpanFieldVisitor<'a> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .push((field.name().to_string(), json!(format!("{value:?}"))));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.push((field.name().to_string(), json!(value)));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.push((field.name().to_string(), json!(value)));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.push((field.name().to_string(), json!(value)));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields.push((field.name().to_string(), json!(value)));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.push((field.name().to_string(), json!(value)));
    }
}

/// Visitor that collects tracing event fields into a serde_json [`Map`].
///
/// The `message` field is mapped to GELF's `short_message`; all other fields
/// are prefixed with `_` per the GELF spec for additional fields.
struct FieldVisitor<'a> {
    fields: &'a mut Map<String, Value>,
}

impl<'a> Visit for FieldVisitor<'a> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let key = field.name();
        let val = format!("{value:?}");
        if key == "message" {
            self.fields.insert("short_message".into(), json!(val));
        } else {
            self.fields.insert(format!("_{key}"), json!(val));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let key = field.name();
        if key == "message" {
            self.fields.insert("short_message".into(), json!(value));
        } else {
            self.fields.insert(format!("_{key}"), json!(value));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(format!("_{}", field.name()), json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(format!("_{}", field.name()), json!(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields
            .insert(format!("_{}", field.name()), json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(format!("_{}", field.name()), json!(value));
    }
}

impl<S> Layer<S> for GelfLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        if let Some(span) = ctx.span(id) {
            let mut fields = Vec::new();
            let mut visitor = SpanFieldVisitor {
                fields: &mut fields,
            };
            attrs.record(&mut visitor);
            span.extensions_mut().insert(SpanFields { fields });
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let mut fields = self.base_fields.clone();

        // Map tracing level to GELF/syslog numeric level
        let level_num = match *event.metadata().level() {
            Level::ERROR => 3,
            Level::WARN => 4,
            Level::INFO => 6,
            Level::DEBUG => 7,
            Level::TRACE => 7,
        };
        fields.insert("level".into(), json!(level_num));

        // Include the tracing level name for TRACE vs DEBUG distinction
        fields.insert(
            "_level".into(),
            json!(event.metadata().level().to_string()),
        );

        // Metadata fields (target, file, line)
        add_metadata_fields(
            &mut fields,
            Some(event.metadata().target()),
            event.metadata().file(),
            event.metadata().line(),
        );

        // Service name
        add_service_field(&mut fields, self.service_name.as_deref());

        // Current span context
        if let Some(span) = ctx.lookup_current() {
            fields.insert("_span_name".into(), json!(span.name()));

            let extensions = span.extensions();
            if let Some(span_fields) = extensions.get::<SpanFields>() {
                for (key, value) in &span_fields.fields {
                    fields.insert(format!("_span_{key}"), value.clone());
                }
            }

            // OTel trace context (only with otel feature)
            #[cfg(feature = "otel")]
            {
                use opentelemetry::trace::TraceContextExt;
                if let Some(otel_data) =
                    extensions.get::<tracing_opentelemetry::OtelData>()
                {
                    let span_ctx =
                        otel_data.parent_cx.span().span_context().clone();
                    if span_ctx.is_valid() {
                        fields.insert(
                            "_trace_id".into(),
                            json!(format!("{:032x}", span_ctx.trace_id())),
                        );
                        fields.insert(
                            "_span_id".into(),
                            json!(format!("{:016x}", span_ctx.span_id())),
                        );
                    }
                }
            }
        }

        // Collect event fields
        let mut visitor = FieldVisitor {
            fields: &mut fields,
        };
        event.record(&mut visitor);

        // GELF requires short_message
        if !fields.contains_key("short_message") {
            fields.insert("short_message".into(), json!(""));
        }

        // Best-effort send -- silently drop on failure
        if let Ok(bytes) = serde_json::to_vec(&Value::Object(fields)) {
            let _ = self.socket.send_to(&bytes, self.addr);
        }
    }
}
