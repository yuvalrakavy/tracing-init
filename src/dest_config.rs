//! Per-destination settings resolution.
//!
//! Stores settings keyed by destination name (`"console"`, `"file"`, `"gelf"`, `"otel"`)
//! or `"*"` for defaults. Specific destination settings override wildcard defaults.

use std::collections::HashMap;
use tracing::Level;
use crate::types::{Format, SpanEvents};

/// Holds per-destination settings set via the builder API.
///
/// Each setting can be set for `"*"` (all destinations) or a specific destination.
/// `resolve_*` methods return the most specific value: destination-specific if set,
/// otherwise wildcard, otherwise `None`.
#[derive(Debug, Clone, Default)]
pub struct DestinationSettings {
    levels: HashMap<String, Level>,
    filters: HashMap<String, String>,
    formats: HashMap<String, Format>,
    ansi: HashMap<String, bool>,
    timestamps: HashMap<String, bool>,
    target: HashMap<String, bool>,
    thread_names: HashMap<String, bool>,
    file_line: HashMap<String, bool>,
    span_events: HashMap<String, SpanEvents>,
}

impl DestinationSettings {
    pub fn new() -> Self { Self::default() }

    pub fn set_level(&mut self, dest: &str, level: Level) { self.levels.insert(dest.to_string(), level); }
    pub fn set_filter(&mut self, dest: &str, filter: &str) { self.filters.insert(dest.to_string(), filter.to_string()); }
    pub fn set_format(&mut self, dest: &str, format: Format) { self.formats.insert(dest.to_string(), format); }
    pub fn set_ansi(&mut self, dest: &str, value: bool) { self.ansi.insert(dest.to_string(), value); }
    pub fn set_timestamps(&mut self, dest: &str, value: bool) { self.timestamps.insert(dest.to_string(), value); }
    pub fn set_target(&mut self, dest: &str, value: bool) { self.target.insert(dest.to_string(), value); }
    pub fn set_thread_names(&mut self, dest: &str, value: bool) { self.thread_names.insert(dest.to_string(), value); }
    pub fn set_file_line(&mut self, dest: &str, value: bool) { self.file_line.insert(dest.to_string(), value); }
    pub fn set_span_events(&mut self, dest: &str, events: SpanEvents) { self.span_events.insert(dest.to_string(), events); }

    pub fn resolve_level(&self, dest: &str) -> Option<Level> { self.levels.get(dest).or_else(|| self.levels.get("*")).copied() }
    pub fn resolve_filter(&self, dest: &str) -> Option<&str> { self.filters.get(dest).or_else(|| self.filters.get("*")).map(|s| s.as_str()) }
    pub fn resolve_format(&self, dest: &str) -> Option<Format> { self.formats.get(dest).or_else(|| self.formats.get("*")).copied() }
    pub fn resolve_ansi(&self, dest: &str) -> Option<bool> { self.ansi.get(dest).or_else(|| self.ansi.get("*")).copied() }
    pub fn resolve_timestamps(&self, dest: &str) -> Option<bool> { self.timestamps.get(dest).or_else(|| self.timestamps.get("*")).copied() }
    pub fn resolve_target(&self, dest: &str) -> Option<bool> { self.target.get(dest).or_else(|| self.target.get("*")).copied() }
    pub fn resolve_thread_names(&self, dest: &str) -> Option<bool> { self.thread_names.get(dest).or_else(|| self.thread_names.get("*")).copied() }
    pub fn resolve_file_line(&self, dest: &str) -> Option<bool> { self.file_line.get(dest).or_else(|| self.file_line.get("*")).copied() }
    pub fn resolve_span_events(&self, dest: &str) -> Option<SpanEvents> { self.span_events.get(dest).or_else(|| self.span_events.get("*")).copied() }
}
