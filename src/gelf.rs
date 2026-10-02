//! Lightweight GELF (Graylog Extended Log Format) layer for [`tracing-subscriber`].
//!
//! Sends JSON-encoded [GELF 1.1](https://go2docs.graylog.org/current/getting_in_log_data/gelf.html)
//! messages over a `std::net::UdpSocket`. The implementation is deliberately simple:
//!
//! - **No async runtime required** -- each event is serialized and sent inline.
//! - **Never waits** -- the socket is non-blocking. A blocking UDP send waits for room in the
//!   socket's send buffer, which a stalled interface queue never makes, holding the thread that
//!   logged; here that send fails at once instead.
//! - **Best-effort delivery, counted loss** -- a send that fails loses its record, as UDP does
//!   on the wire, but the failure is counted and reported like any destination's lost lines
//!   (`sink.rs`). "Delivered" means handed to the kernel; a datagram lost in the network is not
//!   seen.
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
//! | Span fields (full scope, root→leaf, inner wins) | `_span_<field>` |
//! | OTel trace ID (otel feature) | `_trace_id` |
//! | OTel span ID (otel feature) | `_span_id` |
//! | Other fields | `_<field_name>` |
//!
//! Events forwarded from the [`log`] crate (via `tracing-log`'s `LogTracer`,
//! which `tracing_subscriber`'s `init()` installs by default) carry the static
//! metadata target `"log"`; this layer normalizes them so `_target`, `_file`
//! and `_line` reflect the real emitting module, and the internal `log.*`
//! carrier fields are not emitted.

use serde_json::{json, Map, Value};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use tracing::field::{Field, Visit};
use tracing::Level;
use tracing_log::NormalizeEvent;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::sink::{LineCounts, Watch};

/// A [`tracing_subscriber::Layer`] that sends events as GELF messages over UDP.
///
/// Create with [`GelfLayer::new`] and register it with a
/// [`tracing_subscriber::Registry`].
pub struct GelfLayer {
    socket: UdpSocket,
    addr: SocketAddr,
    base_fields: Map<String, Value>,
    service_name: Option<String>,
    counts: Arc<LineCounts>,
}

impl GelfLayer {
    /// Create a new GELF layer that sends to the given `host:port` address.
    ///
    /// The `additional_fields` are included in every GELF message as `_<key>` fields.
    /// The local hostname is automatically resolved and included as the GELF `host` field.
    ///
    /// # Errors
    ///
    /// Returns an error if the address cannot be resolved, or the UDP socket cannot be bound or
    /// made non-blocking.
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
        socket.set_nonblocking(true)?;

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
            counts: Arc::new(LineCounts::default()),
        })
    }

    /// This destination's counts, for the loss monitor.
    pub(crate) fn watch(&self) -> Watch {
        Watch::new("gelf", self.counts.clone(), None)
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
    /// True when the event came through `tracing-log`'s bridge: its
    /// `log.target`/`log.module_path`/`log.file`/`log.line` carrier fields
    /// surface via [`NormalizeEvent`] as proper metadata, so emitting them
    /// as GELF fields would only duplicate it. Native tracing events keep
    /// any (unusual) user field that happens to start with `log.`.
    skip_log_carriers: bool,
}

fn is_log_carrier_field(name: &str) -> bool {
    name.starts_with("log.")
}

impl<'a> Visit for FieldVisitor<'a> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let key = field.name();
        if self.skip_log_carriers && is_log_carrier_field(key) {
            return;
        }
        let val = format!("{value:?}");
        if key == "message" {
            self.fields.insert("short_message".into(), json!(val));
        } else {
            self.fields.insert(format!("_{key}"), json!(val));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let key = field.name();
        if self.skip_log_carriers && is_log_carrier_field(key) {
            return;
        }
        if key == "message" {
            self.fields.insert("short_message".into(), json!(value));
        } else {
            self.fields.insert(format!("_{key}"), json!(value));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if self.skip_log_carriers && is_log_carrier_field(field.name()) {
            return;
        }
        self.fields
            .insert(format!("_{}", field.name()), json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if self.skip_log_carriers && is_log_carrier_field(field.name()) {
            return;
        }
        self.fields
            .insert(format!("_{}", field.name()), json!(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if self.skip_log_carriers && is_log_carrier_field(field.name()) {
            return;
        }
        self.fields
            .insert(format!("_{}", field.name()), json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if self.skip_log_carriers && is_log_carrier_field(field.name()) {
            return;
        }
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

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        // Capture fields recorded after span creation (`field::Empty` +
        // `span.record(...)`). Re-recorded names replace in place so a
        // long-lived span that records a field periodically doesn't grow
        // its extension (and the per-event iteration) without bound.
        if let Some(span) = ctx.span(id) {
            let mut new_fields = Vec::new();
            let mut visitor = SpanFieldVisitor {
                fields: &mut new_fields,
            };
            values.record(&mut visitor);

            let mut extensions = span.extensions_mut();
            if let Some(span_fields) = extensions.get_mut::<SpanFields>() {
                for (key, value) in new_fields {
                    if let Some(slot) = span_fields
                        .fields
                        .iter_mut()
                        .find(|(existing, _)| *existing == key)
                    {
                        slot.1 = value;
                    } else {
                        span_fields.fields.push((key, value));
                    }
                }
            } else {
                extensions.insert(SpanFields { fields: new_fields });
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let mut fields = self.base_fields.clone();

        // Events bridged from the `log` crate carry static metadata target
        // "log"; normalized_metadata() reconstructs the real emitter's
        // target/file/line from the bridge's carrier fields.
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());

        // Map tracing level to GELF/syslog numeric level
        let level_num = match *meta.level() {
            Level::ERROR => 3,
            Level::WARN => 4,
            Level::INFO => 6,
            Level::DEBUG => 7,
            Level::TRACE => 7,
        };
        fields.insert("level".into(), json!(level_num));

        // Include the tracing level name for TRACE vs DEBUG distinction
        fields.insert("_level".into(), json!(meta.level().to_string()));

        // Metadata fields (target, file, line)
        add_metadata_fields(&mut fields, Some(meta.target()), meta.file(), meta.line());

        // Service name
        add_service_field(&mut fields, self.service_name.as_deref());

        // Span context: walk the full scope root→leaf so a field set on an
        // outer span (e.g. panel_id on a connection span) is inherited by
        // events emitted in nested spans; an inner span re-using a field
        // name overrides the outer value.
        if let Some(span) = ctx.lookup_current() {
            fields.insert("_span_name".into(), json!(span.name()));

            for scope_span in span.scope().from_root() {
                let extensions = scope_span.extensions();
                if let Some(span_fields) = extensions.get::<SpanFields>() {
                    for (key, value) in &span_fields.fields {
                        fields.insert(format!("_span_{key}"), value.clone());
                    }
                }
            }

            // OTel trace context (only with otel feature).
            //
            // tracing-opentelemetry 0.32 changed the OtelData layout:
            // `parent_cx` is no longer a public field — it's now inside
            // an `OtelDataState::Builder { parent_cx, .. }` enum variant
            // that's only valid before the span is activated. After
            // activation the data lives in `OtelDataState::Context`
            // and parent_cx is consumed.
            //
            // The right API now is the public `OtelData::trace_id()` /
            // `OtelData::span_id()` accessors (both `Option`-returning),
            // which read from the active context after the span has
            // started. They're populated as soon as the span is entered
            // for the first time (or `OpenTelemetrySpanExt::context()`
            // is called on it), which always happens by the time
            // `on_event` fires for events emitted inside the span.
            //
            // Walk leaf→root and take the first span with a valid trace
            // context, so an inner span the OTel layer skipped (filtered,
            // or not yet activated) doesn't hide an outer span's trace.
            // The all-zero invalid trace_id is what an unparented span
            // looks like when no remote context was attached; emitting it
            // would mislead log correlation tools.
            #[cfg(feature = "otel")]
            for scope_span in span.scope() {
                let extensions = scope_span.extensions();
                let Some(otel_data) = extensions.get::<tracing_opentelemetry::OtelData>() else {
                    continue;
                };
                if let (Some(trace_id), Some(span_id)) = (otel_data.trace_id(), otel_data.span_id())
                {
                    if trace_id != opentelemetry::trace::TraceId::INVALID {
                        fields.insert("_trace_id".into(), json!(format!("{trace_id:032x}")));
                        fields.insert("_span_id".into(), json!(format!("{span_id:016x}")));
                        break;
                    }
                }
            }
        }

        // Collect event fields
        let mut visitor = FieldVisitor {
            fields: &mut fields,
            skip_log_carriers: normalized.is_some(),
        };
        event.record(&mut visitor);

        // GELF requires short_message
        if !fields.contains_key("short_message") {
            fields.insert("short_message".into(), json!(""));
        }

        // Best-effort and never waiting: a record that cannot be sent now is lost, and counted.
        let sent = serde_json::to_vec(&Value::Object(fields))
            .ok()
            .and_then(|bytes| self.socket.send_to(&bytes, self.addr).ok());
        match sent {
            Some(_) => self.counts.note_delivered(),
            None => self.counts.note_lost(),
        }
    }
}

#[cfg(test)]
mod send_tests {
    use super::GelfLayer;
    use std::time::{Duration, Instant};

    /// The socket's mode is what decides whether a send can wait: a blocking UDP socket whose
    /// send buffer is full (a stalled interface queue) waits in `send_to` for room, holding
    /// the thread that logged. A full send buffer cannot be produced over loopback, so the
    /// mode is read through the one call that shows it without one — a receive with nothing
    /// to read returns at once on a non-blocking socket and waits out its timeout otherwise.
    #[test]
    fn the_gelf_socket_never_waits() {
        let layer = GelfLayer::new("127.0.0.1:9", vec![], None).expect("a GELF layer");
        layer
            .socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("a read timeout");
        let started = Instant::now();
        let mut buf = [0u8; 16];
        let received = layer.socket.recv_from(&mut buf);
        let took = started.elapsed();
        assert!(
            took < Duration::from_millis(500),
            "the GELF socket waits: a receive with nothing to read took {took:?} ({received:?}), so a \
             send into a full buffer would hold the thread that logged"
        );
    }

    /// A send that fails loses its record, and the loss is counted for the monitor; a send the
    /// kernel takes is counted as delivered. A record past UDP's 64 KiB limit cannot be sent.
    #[test]
    fn a_failed_send_is_counted_as_lost() {
        use tracing_subscriber::layer::SubscriberExt;

        let listener = std::net::UdpSocket::bind("127.0.0.1:0").expect("a listener");
        let address = listener.local_addr().expect("its address").to_string();
        let layer = GelfLayer::new(&address, vec![], None).expect("a GELF layer");
        let watch = layer.watch();
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("a record that fits");
            tracing::info!(padding = %"x".repeat(70_000), "a record that does not");
        });
        assert_eq!(watch.delivered(), 1, "the record that fits was sent");
        assert_eq!(
            watch.lost(),
            1,
            "the record that could not be sent is counted"
        );
    }
}
