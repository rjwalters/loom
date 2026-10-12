//! The post-wait head/merged re-read decision behind `merge-pr.sh`'s
//! `_revalidate_merge_guards` (#8410, #8896, #8191 slice).
//!
//! `--auto` waits for the head's checks to settle, then re-reads the PR
//! uncached and asks four things of that payload before re-running the label
//! guards: did it merge underneath us, does it name a head at all, did that
//! head move past the SHA the merge is gated on, and — if all clear — what is
//! the label set NOW. In the shell these were three `jq -r` filters plus a
//! compound `if`; each `jq` read an UNPARSEABLE payload as "empty", which is
//! the shape this decision must refuse on, so it is one function here.
//!
//! `jq -r 'EXPR // empty'` semantics are preserved for the shapes a forge
//! sends: `null`/`false` read as absent, a string is taken verbatim, a number
//! or `true` prints as text. A payload that is not JSON at all reads as "not
//! merged, no head" ([`Revalidation::NoHead`]) exactly as the retired
//! `$(... || ...)` chain did, so it refuses rather than passes.

use serde_json::Value;

/// What the shell must do with the re-read payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revalidation {
    /// The PR merged while `--auto` waited: nothing left to guard.
    Merged,
    /// No usable head SHA: the re-read never happened. Refuse — this is a forge
    /// read failure, not evidence about the labels (#8896).
    NoHead,
    /// The head moved past the SHA the merge is gated on (exit 3, re-queue).
    Moved { fresh_sha: String },
    /// Head unchanged: re-run the label guards against these labels.
    Clear { labels: Vec<String> },
}

fn raw(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null | Value::Bool(false) => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(true) => Some("true".to_string()),
        other => Some(other.to_string()),
    }
}

/// Classify the uncached re-read `payload` against `precondition_sha` (empty
/// when the merge has no precondition).
#[must_use]
pub fn revalidate(payload: &str, precondition_sha: &str) -> Revalidation {
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return Revalidation::NoHead;
    };
    if raw(v.get("merged")).as_deref() == Some("true") {
        return Revalidation::Merged;
    }
    let sha = raw(v.pointer("/head/sha")).unwrap_or_default();
    if sha.is_empty() {
        return Revalidation::NoHead;
    }
    if !precondition_sha.is_empty() && sha != precondition_sha {
        return Revalidation::Moved { fresh_sha: sha };
    }
    let labels = v
        .get("labels")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|l| raw(l.get("name"))).collect())
        .unwrap_or_default();
    Revalidation::Clear { labels }
}

#[cfg(test)]
mod tests;
