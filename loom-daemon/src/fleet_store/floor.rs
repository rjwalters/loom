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
//! It is always a JSON string in `fleet.json`.
//!
//! Read from the top level of the store's compiled `fleet.json`
//! ([`super::FLEET_JSON_PATH`], via [`super::compiled`]). Without the key, or
//! without a `fleet.json`, the result is **Absent**; a legacy `repos.yml` is no
//! longer read. A `fleet.json` that is present but not a valid compiled
//! document is **Malformed**.
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

/// The key.
pub const KEY: &str = "loom_min_version";

/// What one snapshot says about the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorRead {
    /// No `fleet.json`, or it lacks the key.
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

/// Read the floor from `snapshot`'s compiled `fleet.json`.
#[must_use]
pub fn read(snapshot: &Snapshot) -> FloorRead {
    match compiled::from_snapshot(snapshot) {
        // `roster()` is the whole top-level document, so this is the top-level
        // `loom_min_version` key of `fleet.json`.
        Ok(Some(c)) => c
            .roster()
            .get(KEY)
            .map_or(FloorRead::Absent, |value| classify(value, super::FLEET_JSON_PATH)),
        Ok(None) => FloorRead::Absent,
        Err(e) => FloorRead::Malformed {
            source: super::FLEET_JSON_PATH,
            detail: format!("{e:#}"),
        },
    }
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
