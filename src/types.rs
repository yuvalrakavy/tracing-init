use std::str::FromStr;

/// Output format for console and file logging layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Default verbose format with all fields.
    Full,
    /// Condensed single-line format.
    Compact,
    /// Multi-line colorful format (console only).
    Pretty,
    /// Structured JSON output.
    Json,
}

impl FromStr for Format {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "full" => Ok(Format::Full),
            "compact" => Ok(Format::Compact),
            "pretty" => Ok(Format::Pretty),
            "json" => Ok(Format::Json),
            other => Err(format!("unknown format: '{other}' (expected full, compact, pretty, or json)")),
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Format::Full => write!(f, "full"),
            Format::Compact => write!(f, "compact"),
            Format::Pretty => write!(f, "pretty"),
            Format::Json => write!(f, "json"),
        }
    }
}

/// OTLP transport protocol.
#[cfg(feature = "otel")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// HTTP/JSON (default, lighter dependency footprint).
    Http,
    /// gRPC via tonic (requires `otel-grpc` feature).
    Grpc,
}

#[cfg(feature = "otel")]
impl FromStr for Transport {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "http" => Ok(Transport::Http),
            "grpc" => Ok(Transport::Grpc),
            other => Err(format!("unknown transport: '{other}' (expected http or grpc)")),
        }
    }
}

#[cfg(feature = "otel")]
impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Transport::Http => write!(f, "http"),
            Transport::Grpc => write!(f, "grpc"),
        }
    }
}

bitflags::bitflags! {
    /// Which span lifecycle events to log.
    ///
    /// Maps to `tracing_subscriber::fmt::format::FmtSpan`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SpanEvents: u8 {
        /// Log when a span is created.
        const NEW    = 0b001;
        /// Log when a span is closed/dropped.
        const CLOSE  = 0b010;
        /// Log when a span is entered (becomes the active span).
        const ACTIVE = 0b100;
        /// No span events.
        const NONE   = 0b000;
        /// All span events.
        const ALL    = Self::NEW.bits() | Self::CLOSE.bits() | Self::ACTIVE.bits();
    }
}

impl FromStr for SpanEvents {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim().to_lowercase();
        match trimmed.as_str() {
            "none" => return Ok(SpanEvents::NONE),
            "all" => return Ok(SpanEvents::ALL),
            _ => {}
        }

        let mut result = SpanEvents::NONE;
        for part in trimmed.split(',') {
            let part = part.trim();
            match part {
                "new" => result |= SpanEvents::NEW,
                "close" => result |= SpanEvents::CLOSE,
                "active" => result |= SpanEvents::ACTIVE,
                other => return Err(format!("unknown span event: '{other}' (expected new, close, active, none, or all)")),
            }
        }
        Ok(result)
    }
}

impl std::fmt::Display for SpanEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if *self == SpanEvents::NONE {
            return write!(f, "none");
        }
        if *self == SpanEvents::ALL {
            return write!(f, "all");
        }
        let mut parts = Vec::new();
        if self.contains(SpanEvents::NEW) { parts.push("new"); }
        if self.contains(SpanEvents::CLOSE) { parts.push("close"); }
        if self.contains(SpanEvents::ACTIVE) { parts.push("active"); }
        write!(f, "{}", parts.join(","))
    }
}

impl SpanEvents {
    /// Convert to `tracing_subscriber::fmt::format::FmtSpan`.
    pub fn to_fmt_span(self) -> tracing_subscriber::fmt::format::FmtSpan {
        use tracing_subscriber::fmt::format::FmtSpan;
        let mut result = FmtSpan::NONE;
        if self.contains(SpanEvents::NEW) { result |= FmtSpan::NEW; }
        if self.contains(SpanEvents::CLOSE) { result |= FmtSpan::CLOSE; }
        if self.contains(SpanEvents::ACTIVE) { result |= FmtSpan::ACTIVE; }
        result
    }
}
