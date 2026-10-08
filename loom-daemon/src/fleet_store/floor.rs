//! The fleet-wide minimum Loom version, `loom_min_version` (#10711, part of
//! #10698).
//!
//! # Contract
//!
//! A top-level key holding an `"X.Y.Z"` string:
//!
//! ```yaml
//! loom_min_version: "0.19.830"
//! ```
//!
//! In `fleet.json` it is always a JSON string. In `repos.yml` quote it; an
//! unquoted `X.Y.Z` is also read (YAML parses it as a string), while an
//! unquoted `X.Y` is a number and is refused.
//!
//! Read from the top level of the store's compiled `fleet.json`
//! ([`super::FLEET_JSON_PATH`], via [`super::compiled`]) when that file is
//! present, and otherwise from the top level of `repos.yml`
//! ([`super::ROSTER_PATH`]). A `fleet.json` that is present is the only
//! source: without the key the result is **Absent**, and `repos.yml` is not
//! consulted. One that is present but not a valid compiled document is
//! **Malformed**, again with no fallback. The `repos.yml` fallback lives
//! entirely in [`from_repos_yml`] so the legacy-fallback removal (#10705
//! follow-up) can delete it in one place; [`super::roster::parse`] is
//! deliberately not widened to know about the key.
//!
//! # Outcomes
//!
//! - **Absent**: no floor.
//! - **Valid** canonical `X.Y.Z`: three dot-separated decimal integers, no
//!   whitespace, no sign, no leading zeros (a component is `0` or starts with
//!   `1`-`9`). That floor.
//! - **Malformed** (not a string, not canonical `X.Y.Z`, or a source file that
//!   cannot be read as above): never read as "no floor". The caller keeps its
//!   last good floor and alerts (see [`crate::fleet_sync`]). `"01.2.3"` and
//!   `" 0.19.830 "` are Malformed rather than silently reinterpreted: the
//!   floor drives rolls and alerts, so a typo should be seen.
//!
//! This module is pure: it reads a [`Snapshot`] and decides.

use serde_json::Value;

use super::{compiled, fetch::Snapshot};

/// The key, in both sources.
pub const KEY: &str = "loom_min_version";

/// What one snapshot says about the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorRead {
    /// Neither source carries the key.
    Absent,
    /// A well-formed floor.
    Valid {
        /// The floor, canonical `X.Y.Z`.
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

/// Read the floor from `snapshot`: the compiled `fleet.json` when present,
/// else the top level of `repos.yml`.
#[must_use]
pub fn read(snapshot: &Snapshot) -> FloorRead {
    match compiled::from_snapshot(snapshot) {
        // `roster()` is the whole top-level document, so this is the top-level
        // `loom_min_version` key of `fleet.json`.
        Ok(Some(c)) => {
            return c
                .roster()
                .get(KEY)
                .map_or(FloorRead::Absent, |value| classify(value, super::FLEET_JSON_PATH))
        }
        Ok(None) => {}
        Err(e) => {
            return FloorRead::Malformed {
                source: super::FLEET_JSON_PATH,
                detail: format!("{e:#}"),
            }
        }
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

/// The raw `loom_min_version` value at the top level of `repos.yml`, `None`
/// when the key is absent. `Err` when the file is not a YAML mapping.
///
/// The legacy source, used only when the store has no `fleet.json`; the
/// legacy-fallback removal (#10705 follow-up) deletes this function together
/// with its call in [`read`].
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

/// Validate a raw `loom_min_version` value as a canonical `X.Y.Z` string
/// (no whitespace, no leading zeros).
pub fn validate(value: &Value) -> Result<String, String> {
    let Some(raw) = value.as_str() else {
        return Err(format!(
            "`{KEY}` must be an \"X.Y.Z\" string, got {}",
            truncate(&value.to_string(), 60)
        ));
    };
    match parse_triple(raw) {
        Some((major, minor, patch)) => Ok(format!("{major}.{minor}.{patch}")),
        None => Err(format!(
            "`{KEY}` must be canonical \"X.Y.Z\" (three decimal integers, no whitespace, no \
             leading zeros), got \"{}\"",
            truncate(raw, 60)
        )),
    }
}

/// Parse canonical `X.Y.Z` into its three components. `None` for anything else
/// (whitespace, pre-release or build suffixes, a leading `v` or `+`, leading
/// zeros such as `01`, missing or extra parts). A component is `0` or starts
/// with `1`-`9`.
#[must_use]
pub fn parse_triple(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.split('.');
    let mut next = || -> Option<u64> {
        let p = parts.next()?;
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if p.len() > 1 && p.starts_with('0') {
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
