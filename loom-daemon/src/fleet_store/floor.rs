//! The fleet-wide minimum Loom version, `loom_min_version` (#10711, part of
//! #10698).
//!
//! # Contract
//!
//! A top-level key holding a quoted `"X.Y.Z"` string:
//!
//! ```yaml
//! loom_min_version: "0.19.830"
//! ```
//!
//! Read from the store's compiled `fleet.json` ([`super::FLEET_JSON_PATH`])
//! when that file carries the key, and otherwise from the top level of
//! `repos.yml` ([`super::ROSTER_PATH`]). The fallback lives entirely in
//! [`from_repos_yml`] so #10705, which makes `fleet.json` the only source, can
//! delete it in one place; [`super::roster::parse`] is deliberately not widened
//! to know about the key.
//!
//! # Outcomes
//!
//! - **Absent** from both sources: no floor.
//! - **Valid** `X.Y.Z` (three dot-separated decimal integers): that floor.
//! - **Malformed** (not a string, not `X.Y.Z`, or a source file that cannot
//!   be parsed at all): never read as "no floor". The caller keeps its last
//!   good floor and alerts (see [`crate::fleet_sync`]).
//!
//! This module is pure: it reads a [`Snapshot`] and decides. Nothing consumes
//! the floor yet; the comparison against the running version is a later
//! #10698 step.

use serde_json::Value;

use super::fetch::Snapshot;

/// The key, in both sources.
pub const KEY: &str = "loom_min_version";

/// What one snapshot says about the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorRead {
    /// Neither source carries the key.
    Absent,
    /// A well-formed floor.
    Valid {
        /// The floor, normalised to `X.Y.Z`.
        version: String,
        /// The store path it was read from.
        source: &'static str,
    },
    /// A key (or a source file) that could not be read as a floor.
    Malformed {
        /// The store path at fault.
        source: &'static str,
        /// Why.
        detail: String,
    },
}

/// Read the floor from `snapshot`: `fleet.json` first, then the top level of
/// `repos.yml`.
#[must_use]
pub fn read(snapshot: &Snapshot) -> FloorRead {
    match source_text(snapshot, super::FLEET_JSON_PATH) {
        Err(read) => return read,
        Ok(Some(text)) => match from_fleet_json(&text) {
            Ok(Some(value)) => return classify(&value, super::FLEET_JSON_PATH),
            Ok(None) => {}
            Err(detail) => {
                return FloorRead::Malformed {
                    source: super::FLEET_JSON_PATH,
                    detail,
                }
            }
        },
        Ok(None) => {}
    }
    match source_text(snapshot, super::ROSTER_PATH) {
        Err(read) => read,
        Ok(None) => FloorRead::Absent,
        Ok(Some(text)) => match from_repos_yml(&text) {
            Ok(Some(value)) => classify(&value, super::ROSTER_PATH),
            Ok(None) => FloorRead::Absent,
            Err(detail) => FloorRead::Malformed {
                source: super::ROSTER_PATH,
                detail,
            },
        },
    }
}

fn source_text(snapshot: &Snapshot, path: &'static str) -> Result<Option<String>, FloorRead> {
    snapshot.text(path).map_err(|e| FloorRead::Malformed {
        source: path,
        detail: format!("{e:#}"),
    })
}

/// The raw `loom_min_version` value at the top level of `fleet.json`, `None`
/// when the key is absent. `Err` when the file is not a JSON object.
fn from_fleet_json(text: &str) -> Result<Option<Value>, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON ({e})"))?;
    let top = doc
        .as_object()
        .ok_or_else(|| "top level must be an object".to_string())?;
    Ok(top.get(KEY).cloned())
}

/// The raw `loom_min_version` value at the top level of `repos.yml`, `None`
/// when the key is absent. `Err` when the file is not a YAML mapping.
///
/// The interim source until #10705 compiles `fleet.json`; that issue deletes
/// this function together with its call in [`read`].
fn from_repos_yml(text: &str) -> Result<Option<Value>, String> {
    let doc = super::yaml::parse(text).map_err(|e| format!("not valid YAML ({e:#})"))?;
    let top = doc
        .as_object()
        .ok_or_else(|| "top level must be a mapping".to_string())?;
    Ok(top.get(KEY).cloned())
}

fn classify(value: &Value, source: &'static str) -> FloorRead {
    match validate(value) {
        Ok(version) => FloorRead::Valid { version, source },
        Err(detail) => FloorRead::Malformed { source, detail },
    }
}

/// Validate a raw `loom_min_version` value as an `X.Y.Z` string.
pub fn validate(value: &Value) -> Result<String, String> {
    let Some(raw) = value.as_str() else {
        return Err(format!(
            "`{KEY}` must be a quoted \"X.Y.Z\" string, got {}",
            truncate(&value.to_string(), 60)
        ));
    };
    let trimmed = raw.trim();
    match parse_triple(trimmed) {
        Some((major, minor, patch)) => Ok(format!("{major}.{minor}.{patch}")),
        None => Err(format!(
            "`{KEY}` must be \"X.Y.Z\" (three decimal integers), got \"{}\"",
            truncate(trimmed, 60)
        )),
    }
}

/// Parse `X.Y.Z` into its three components. `None` for anything else
/// (pre-release or build suffixes, a leading `v`, missing or extra parts).
#[must_use]
pub fn parse_triple(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.split('.');
    let mut next = || -> Option<u64> {
        let p = parts.next()?;
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        p.parse().ok()
    };
    let triple = (next()?, next()?, next()?);
    parts.next().is_none().then_some(triple)
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
#[path = "tests/floor_tests.rs"]
mod tests;
