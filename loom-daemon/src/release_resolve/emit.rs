//! The `--resolve-json` object (epic #7810, PR 5).
//!
//! One JSON object, 14 fields, **no field ever omitted** — a consumer relies on
//! the shape. Every value is a JSON string or `null`; `null` means *could not
//! be determined*, never a fabricated default.
//!
//! This is the contract `loom-daemon-update.sh --resolve-json` has published
//! since #7609, and the shell now delegates here rather than building it a
//! second time.

use super::{Resolution, Resolved};
use serde_json::{json, Value};

/// Render a resolution as the published object.
///
/// `ok` and `reason` carry the outcome: `ok:true` with an empty reason, or
/// `ok:false` with the operator-facing text saying why. `ok:false` is a normal
/// answer — no Releases yet, an unreachable API, an unbuilt platform — not an
/// error, and the caller's exit code (0/1) says the same thing.
#[must_use]
pub fn to_json(resolution: &Resolution) -> Value {
    match resolution {
        Resolution::Resolved(r) => object(true, "", r),
        Resolution::Unresolved(reason) => object(false, reason, &Resolved::default()),
    }
}

fn object(ok: bool, reason: &str, r: &Resolved) -> Value {
    // `str_or_null`: an EMPTY string is still a string in this contract — only
    // an absent value is null. Collapsing "" to null would tell a consumer the
    // field could not be determined when it was determined to be empty.
    let s = |v: &str| -> Value {
        if v.is_empty() {
            Value::Null
        } else {
            json!(v)
        }
    };
    let o = |v: &Option<String>| -> Value { v.as_deref().map_or(Value::Null, |x| json!(x)) };

    json!({
        "ok": ok,
        "reason": s(reason),
        "repo": s(&r.repo),
        "target": s(&r.target),
        "tag": s(&r.tag),
        "version": s(&r.version),
        "published_at": o(&r.published_at),
        "asset_sha256": o(&r.asset_sha256),
        "installed_bin": r.installed_bin.as_ref().map_or(Value::Null, |p| json!(p.to_string_lossy())),
        "installed_version": o(&r.installed_version),
        "installed_commit": o(&r.installed_commit),
        "installed_sha256": o(&r.installed_sha256),
        "source_version": o(&r.source_version),
        "source_commit": o(&r.source_commit),
    })
}

/// The field names this object always carries, in order — the shape a consumer
/// relies on.
pub const FIELDS: [&str; 14] = [
    "ok",
    "reason",
    "repo",
    "target",
    "tag",
    "version",
    "published_at",
    "asset_sha256",
    "installed_bin",
    "installed_version",
    "installed_commit",
    "installed_sha256",
    "source_version",
    "source_commit",
];

#[cfg(test)]
mod tests;
