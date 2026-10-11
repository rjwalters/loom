//! `doctor-attempt` / `doctor-rejection` / `doctor-route` (#11359): durable
//! GLM-Doctor rejection state kept in the sweep checkpoint's
//! `doctor_escalation` key. Decision logic is `loom_daemon::doctor_escalation`.
//!
//! The state lives in the issue checkpoint when one exists. Mode C can run a
//! Doctor with no checkpoint yet (the first `doctor-done` is written after the
//! attempt) or with no unambiguous closing issue at all, so the same state is
//! otherwise kept in a PR-scoped `pr-<P>-doctor.json` beside the checkpoints.
//! That file is not a phase checkpoint: it records no phase, `list` ignores
//! it, and the first issue `write` carries its state into the checkpoint.
use std::io::Write;
use std::path::{Path, PathBuf};

use loom_daemon::doctor_escalation::{self as de, Rejection, Route, State};
use serde_json::{json, Value};

use super::{invalid, io_error, number, persist, Result};

const KEY: &str = "doctor_escalation";

fn pr_path(dir: &Path, pr: u64) -> PathBuf {
    dir.join(format!("pr-{pr}-doctor.json"))
}

fn state_of(path: &Path) -> Option<Value> {
    load(path).ok().flatten().and_then(|r| r.get(KEY).cloned())
}

fn remove_pr_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(e)),
    }
}

/// Checkpoint `write` rebuilds the whole record, so carry the escalation state
/// over it, adopting PR-scoped state recorded before the checkpoint existed
/// (the PR file is retired once adopted, so `delete` has one store to reset).
/// A new lineage (Curator/Builder completion) resets it deliberately.
pub(super) fn carry_forward(target: &Path, record: &mut Value) -> Result<()> {
    let scoped = record["pr_number"]
        .as_u64()
        .zip(target.parent())
        .map(|(pr, dir)| pr_path(dir, pr));
    if matches!(record["phase"].as_str(), Some("curator-done" | "builder-done")) {
        return scoped.as_deref().map_or(Ok(()), remove_pr_file);
    }
    let pr_scoped = scoped.as_deref().and_then(state_of);
    if let Some(state) = state_of(target).or(pr_scoped) {
        record[KEY] = state;
        if let Some(path) = scoped.as_deref() {
            remove_pr_file(path)?;
        }
    }
    Ok(())
}

/// `delete ISSUE [--pr P]` ends the lineage in both stores. `P` defaults to the
/// checkpoint's `pr_number`; pass `--pr` when no checkpoint was ever written
/// (history then lives only in the PR-scoped file).
pub(super) fn reset_on_delete(dir: &Path, target: &Path, args: &[String]) -> Result<()> {
    let pr = match args {
        // Best effort: an unreadable checkpoint names no PR, and must stay deletable.
        [] => load(target)
            .ok()
            .flatten()
            .and_then(|r| r["pr_number"].as_u64()),
        [flag, value] if flag == "--pr" => Some(u64::from(number(value)?)),
        _ => return Err(invalid("delete takes ISSUE [--pr P]")),
    };
    pr.map_or(Ok(()), |pr| remove_pr_file(&pr_path(dir, pr)))
}

fn load(target: &Path) -> Result<Option<Value>> {
    match std::fs::read(target) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(io_error),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(e)),
    }
}

fn flags(args: &[String]) -> Result<std::collections::HashMap<&str, &str>> {
    let mut map = std::collections::HashMap::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .ok_or_else(|| invalid("option requires a value"))?;
        match flag.as_str() {
            "--pr" | "--head" | "--runtime" | "--profile" | "--attempt" | "--cap" | "--kind" => {
                map.insert(flag.as_str(), value.as_str());
            }
            _ => return Err(invalid("unknown option")),
        }
    }
    Ok(map)
}

fn required<'a>(f: &'a std::collections::HashMap<&str, &str>, name: &str) -> Result<&'a str> {
    f.get(name)
        .copied()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid(&format!("{name} is required")))
}

fn head_value(value: &str) -> Result<&str> {
    if value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(value)
    } else {
        Err(invalid("--head must be a commit SHA"))
    }
}

pub(super) fn run(
    root: &Path,
    dir: &Path,
    issue_target: Option<&Path>,
    verb: &str,
    args: &[String],
    out: &mut impl Write,
) -> Result<()> {
    let f = flags(args)?;
    let pr = u64::from(number(required(&f, "--pr")?)?);
    let pr_target = pr_path(dir, pr);
    // The issue checkpoint wins when it exists; otherwise the PR-scoped file.
    let issue_record = match issue_target {
        Some(t) => load(t)?,
        None => None,
    };
    let (target, mut record) = match (issue_target, issue_record) {
        (Some(t), Some(r)) => (t.to_owned(), r),
        _ => (pr_target.clone(), load(&pr_target)?.unwrap_or_else(|| json!({}))),
    };
    let mut state: State = record
        .get(KEY)
        .cloned()
        .or_else(|| state_of(&pr_target))
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    match verb {
        "route" => {
            let attempt: u32 = number(required(&f, "--attempt")?)?;
            let cap: u32 = number(required(&f, "--cap")?)?;
            let line = match de::route(&state, pr, attempt, cap) {
                Route::Preference => "route=preference".to_owned(),
                Route::CapExhausted => "route=cap-exhausted".to_owned(),
                Route::Claude { reason } => format!("route=claude reason={reason}"),
            };
            return writeln!(out, "{line}").map_err(io_error);
        }
        "clear" => {
            // PR merged or parked for good: drop its history everywhere.
            if let Some(obj) = record.as_object_mut().filter(|_| target != pr_target) {
                if obj.remove(KEY).is_some() {
                    persist(dir, &target, &record)?;
                }
            }
            return match std::fs::remove_file(&pr_target) {
                Ok(()) => writeln!(out, "cleared pr={pr}").map_err(io_error),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    writeln!(out, "cleared pr={pr}").map_err(io_error)
                }
                Err(e) => Err(io_error(e)),
            };
        }
        "attempt" | "rejection" => {}
        _ => return Err(invalid("unknown doctor-* verb")),
    }
    let head = head_value(required(&f, "--head")?)?;
    let mut changed = true;
    let line = if verb == "attempt" {
        let glm = de::is_glm(required(&f, "--runtime")?, f.get("--profile").copied());
        let rebase = match f.get("--kind").copied().unwrap_or("fix") {
            "fix" => false,
            "rebase" => true,
            _ => return Err(invalid("--kind must be fix or rebase")),
        };
        de::record_doctor(&mut state, pr, head, glm, rebase);
        format!("recorded glm={glm} kind={}", if rebase { "rebase" } else { "fix" })
    } else {
        let k = de::parse_k(&loom_daemon::config_resolver::resolve_effective_config(root))
            .map_err(|m| (78, m))?;
        match de::record_rejection(&mut state, pr, head, k) {
            Rejection::Counted { count, escalated } => {
                format!("counted count={count} k={k} escalated={escalated}")
            }
            Rejection::Replay => {
                changed = false;
                format!("replay count={} k={k}", state.rejections)
            }
            Rejection::NotQualifying(why) => {
                changed = false;
                format!("not-counted {why}")
            }
        }
    };
    // Identity and count are one record, one atomic write.
    if changed {
        record[KEY] = json!(state);
        persist(dir, &target, &record)?;
    }
    writeln!(out, "{line}").map_err(io_error)
}
