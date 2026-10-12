//! Drop a superseded TWIN workflow run's check-runs from a head's rollup
//! (#11097).
//!
//! # The incident
//!
//! One push to a PR branch sometimes creates two runs of the same workflow
//! for the same head SHA in the same second. `GET commits/{sha}/check-runs`
//! returns the check-runs of BOTH runs' check suites, and a completed job
//! keeps its conclusion even after its run is cancelled. So the twin's red or
//! cancelled jobs sat in the rollup next to the surviving run's green ones,
//! and [`super::check_runs_rollup::classify`] (which de-duplicates by name
//! only) reported them as failing: a green tree refused as red.
//!
//! # The rule
//!
//! Workflow runs on the head are grouped by `(workflow_id, event, PR
//! numbers)` — a twin shares all three. Collapsing by check NAME alone would
//! be wrong: two different workflows may share a job name, and hiding one of
//! them would hide a real failure. Within a group of two or more runs:
//!
//! - the **authority** is the newest run (`created_at`, tie-break `id`) that
//!   is `completed` and did not conclude `cancelled` — a cancelled run is not
//!   a verdict about the commit;
//! - a group with no such run supersedes nothing (today's behaviour);
//! - a `completed` run is **superseded** when it is older than the authority
//!   or concluded `cancelled`;
//! - a run that is not `completed` is never superseded, so a still-running
//!   twin keeps the wait pending rather than letting an older verdict settle
//!   it.
//!
//! Then every check-run whose check suite belongs to a superseded run is
//! dropped. A check-run whose suite maps to no run (a third-party app, or a
//! payload without suite ids) is always kept.
//!
//! # Fail direction
//!
//! Dropping rows is the only way this module can make a rollup look better,
//! so every doubt drops nothing: an unreadable or truncated runs payload, or
//! a run record missing any field the rule reads, is an `Err`, and the caller
//! classifies the rollup exactly as before. Nothing here cancels a run
//! (`ci-principles.md`: a started run is never cancelled; this only reads).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// Conclusions that count as success for a `completed` check-run.
const GOOD: [&str; 3] = ["success", "neutral", "skipped"];

/// The check-suite id a check-run row belongs to: GitHub's raw
/// `check_suite.id`, or the flat `check_suite_id` `forge_get_check_runs`
/// projects.
fn suite_of(row: &Value) -> Option<u64> {
    row.pointer("/check_suite/id")
        .and_then(Value::as_u64)
        .or_else(|| row.get("check_suite_id").and_then(Value::as_u64))
}

/// Whether `rows` (a `check_runs` array) could contain twin residue worth a
/// workflow-runs read: some check NAME appears under two or more check
/// suites, and one of those rows is not terminal-success. Everything else
/// (the common case) needs no extra read, because dropping rows could not
/// change the failing set.
#[must_use]
pub fn needs_runs(rows: &[Value]) -> bool {
    let mut suites: BTreeMap<&str, BTreeSet<u64>> = BTreeMap::new();
    let mut bad: BTreeSet<&str> = BTreeSet::new();
    for row in rows {
        let (Some(name), Some(suite)) = (row.get("name").and_then(Value::as_str), suite_of(row))
        else {
            continue;
        };
        suites.entry(name).or_default().insert(suite);
        let done = row.get("status").and_then(Value::as_str) == Some("completed");
        let conclusion = row.get("conclusion").and_then(Value::as_str);
        if done && !conclusion.is_some_and(|c| GOOD.contains(&c)) {
            bad.insert(name);
        }
    }
    bad.iter()
        .any(|n| suites.get(n).is_some_and(|s| s.len() > 1))
}

/// One workflow run, as the rule reads it.
struct Run {
    suite: u64,
    key: (u64, String, Vec<u64>),
    order: (String, u64),
    completed: bool,
    cancelled: bool,
}

fn run_of(i: usize, v: &Value) -> Result<Run, String> {
    let u = |k: &str| {
        v.get(k)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("workflow run {i} has no integer `{k}`"))
    };
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| format!("workflow run {i} has no string `{k}`"))
    };
    let mut prs: Vec<u64> = match v.get("pull_requests") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|p| p.get("number").and_then(Value::as_u64))
            .collect::<Option<_>>()
            .ok_or_else(|| format!("workflow run {i} has an unreadable `pull_requests`"))?,
        Some(_) => return Err(format!("workflow run {i} has an unreadable `pull_requests`")),
    };
    prs.sort_unstable();
    prs.dedup();
    let status = s("status")?;
    Ok(Run {
        suite: u("check_suite_id")?,
        key: (u("workflow_id")?, s("event")?, prs),
        order: (s("created_at")?, u("id")?),
        completed: status == "completed",
        cancelled: v.get("conclusion").and_then(Value::as_str) == Some("cancelled"),
    })
}

/// The check-suite ids of superseded runs in a `GET actions/runs?head_sha=`
/// payload, considering only runs whose `head_sha` is exactly `sha`.
///
/// # Errors
///
/// When the payload is unreadable or truncated, or any run on `sha` lacks a
/// field the rule reads — the caller then drops nothing.
pub fn superseded_suites(payload: &Value, sha: &str) -> Result<BTreeSet<u64>, String> {
    let arr = payload
        .get("workflow_runs")
        .and_then(Value::as_array)
        .ok_or("workflow runs payload has no `workflow_runs` array")?;
    match payload.get("total_count").and_then(Value::as_u64) {
        Some(n) if n as usize <= arr.len() => {}
        Some(n) => return Err(format!("workflow runs read truncated ({} of {n})", arr.len())),
        None => return Err("workflow runs payload has no `total_count`".into()),
    }
    let mut groups: BTreeMap<(u64, String, Vec<u64>), Vec<Run>> = BTreeMap::new();
    for (i, v) in arr.iter().enumerate() {
        if sha.is_empty() || v.get("head_sha").and_then(Value::as_str) != Some(sha) {
            continue;
        }
        let run = run_of(i, v)?;
        groups.entry(run.key.clone()).or_default().push(run);
    }
    let mut out = BTreeSet::new();
    for runs in groups.values().filter(|g| g.len() > 1) {
        let Some(authority) = runs
            .iter()
            .filter(|r| r.completed && !r.cancelled)
            .map(|r| &r.order)
            .max()
        else {
            continue;
        };
        out.extend(
            runs.iter()
                .filter(|r| r.completed && (r.cancelled || r.order < *authority))
                .map(|r| r.suite),
        );
    }
    Ok(out)
}

/// Remove every row of `doc["check_runs"]` whose suite is in `superseded`,
/// lowering `total_count` by the number removed. Returns that number.
pub fn drop_superseded(doc: &mut Value, superseded: &BTreeSet<u64>) -> usize {
    if superseded.is_empty() {
        return 0;
    }
    let Some(Value::Array(rows)) = doc.get_mut("check_runs") else {
        return 0;
    };
    let before = rows.len();
    rows.retain(|r| !suite_of(r).is_some_and(|s| superseded.contains(&s)));
    let removed = before - rows.len();
    if let Some(n) = doc.get("total_count").and_then(Value::as_u64) {
        doc["total_count"] = Value::from(n.saturating_sub(removed as u64));
    }
    removed
}

#[cfg(test)]
mod tests;
