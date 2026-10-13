//! Validation for the native `loom-daemon telemetry append` entry point.
//!
//! Shell producers (T5) pipe the record they just wrote to their legacy log;
//! everything they could get wrong is refused here, in Rust, so the shell call
//! site stays one line with no policy of its own:
//!
//! - the input is bounded ([`MAX_INPUT_BYTES`], the journal's own entry cap);
//! - it is exactly one JSON object;
//! - for envelope streams (`sweep`, `role_tick`, `shell`) it has the
//!   `TelemetryEnvelope` shape: integer `schema_version`, RFC 3339
//!   `emitted_at`, string `host_id`, and a `record` object with a `kind`;
//! - the journal `kind` and natural `key` are short and printable.

use serde_json::Value;

use super::{ShadowRecord, TelemetryStream};
use crate::journal::MAX_ENTRY_BYTES;

/// Largest accepted input. The encoded journal line adds its envelope, so the
/// core's [`MAX_ENTRY_BYTES`] check still applies after this one.
pub const MAX_INPUT_BYTES: usize = MAX_ENTRY_BYTES;
const MAX_KIND_CHARS: usize = 128;
const MAX_KEY_CHARS: usize = 256;

/// Validate `input` for `stream` and build the record to append.
///
/// # Errors
///
/// A human-readable reason the input was refused.
pub fn validate(
    stream: TelemetryStream,
    input: &[u8],
    kind: Option<&str>,
    key: Option<&str>,
) -> Result<ShadowRecord, String> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(format!("input is {} bytes; the cap is {MAX_INPUT_BYTES}", input.len()));
    }
    let text = std::str::from_utf8(input).map_err(|_| "input is not UTF-8".to_owned())?;
    let data: Value = serde_json::from_str(text.trim())
        .map_err(|e| format!("input is not a single JSON value: {e}"))?;
    if !data.is_object() {
        return Err("input must be a JSON object".to_owned());
    }
    let record_kind = if stream.carries_envelope() {
        Some(check_envelope(&data)?)
    } else {
        data.get("kind").and_then(Value::as_str).map(str::to_owned)
    };
    let kind = match (kind, record_kind) {
        (Some(kind), _) => kind.to_owned(),
        (None, Some(kind)) => kind,
        (None, None) => return Err("no record kind: pass --kind".to_owned()),
    };
    check_token("kind", &kind, MAX_KIND_CHARS, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
    })?;
    if let Some(key) = key {
        check_token("key", key, MAX_KEY_CHARS, |c| !c.is_control())?;
    }
    Ok(ShadowRecord {
        kind,
        key: key.map(str::to_owned),
        data,
    })
}

fn check_envelope(data: &Value) -> Result<String, String> {
    if !data.get("schema_version").is_some_and(Value::is_u64) {
        return Err("envelope needs an integer schema_version".to_owned());
    }
    let emitted = data
        .get("emitted_at")
        .and_then(Value::as_str)
        .ok_or("envelope needs a string emitted_at")?;
    chrono::DateTime::parse_from_rfc3339(emitted)
        .map_err(|e| format!("emitted_at is not RFC 3339: {e}"))?;
    if !data.get("host_id").is_some_and(Value::is_string) {
        return Err("envelope needs a string host_id".to_owned());
    }
    data.pointer("/record/kind")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "envelope needs a record object with a string kind".to_owned())
}

fn check_token(
    what: &str,
    value: &str,
    max_chars: usize,
    allowed: impl Fn(char) -> bool,
) -> Result<(), String> {
    if value.is_empty() || value.chars().count() > max_chars {
        return Err(format!("{what} must be 1..={max_chars} characters"));
    }
    if !value.chars().all(allowed) {
        return Err(format!("{what} {value:?} has a disallowed character"));
    }
    Ok(())
}
