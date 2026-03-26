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
//! | Module path | `_module_path` |
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
        })
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
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
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

        // Source location metadata
        if let Some(file) = event.metadata().file() {
            fields.insert("_file".into(), json!(file));
        }
        if let Some(line) = event.metadata().line() {
            fields.insert("_line".into(), json!(line));
        }
        if let Some(module) = event.metadata().module_path() {
            fields.insert("_module_path".into(), json!(module));
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
