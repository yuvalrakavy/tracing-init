//! The lanes' JSON Schemas (`schemas/` at the repository's root), as the suite's assertions use
//! them: every payload the bridge publishes is checked against its lane's.

use std::path::PathBuf;

use serde_json::Value;

/// The schemas' directory.
pub fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("schemas")
}

/// The schema of `lane` (`error`, `state`, `event`, …).
pub fn load(lane: &str) -> Value {
    let path = dir().join(format!("{lane}.schema.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("the schema {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("the schema {} is not JSON: {e}", path.display()))
}

/// Whether `instance` conforms to `lane`'s schema; the first violation when not.
pub fn check(lane: &str, instance: &Value) -> Result<(), String> {
    let schema = load(lane);
    jsonschema::validate(&schema, instance).map_err(|e| format!("{lane}: {e} (at {})", e.instance_path))
}

/// A payload as its lane's schema sees it: JSON, or — for the plain-text lanes (`Active`,
/// `Status`, `Version`) — the text as a JSON string.
pub fn instance(lane: &str, payload: &[u8]) -> Result<Value, String> {
    match lane {
        "active" | "status" | "version" => {
            String::from_utf8(payload.to_vec()).map(Value::String).map_err(|e| format!("{lane}: not text: {e}"))
        }
        _ => serde_json::from_slice(payload).map_err(|e| format!("{lane}: not JSON: {e}")),
    }
}
