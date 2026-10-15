//! The verdict a Judge wrote, handed back to the daemon that closes its span
//! (#12029).
//!
//! `post-verdict.sh` ends a verdict with `loom-daemon forge verdict-labels
//! <pr> --sha <head> --verdict <v>`. When the label transition is verified,
//! that command calls [`record`], which appends `{pr_number, head_sha,
//! verdict, at}` to a local file and nothing else: no forge I/O, and a
//! failure never changes the command's result. Two destinations, each only
//! when the launching process gave the child one:
//!
//! - **a role-runner tick**: the sibling of the [`PR_FILE_ENV`] claim
//!   handoff (`<execution>.verdicts` next to `<execution>.pr`), read back by
//!   `role_invocation` ([`take_tick`]);
//! - **a sweep phase**: the sibling of the inherited trace-context file
//!   ([`CONTEXT_FILE_ENV`], `<execution>.verdicts`), read by the checkpoint
//!   close ([`for_pr`]) and removed when the execution finishes.
//!
//! The verdict vocabulary is the PR marker's (`approved`,
//! `changes-requested`) in snake_case. A refused approval never reaches
//! `verdict-labels --sha`, so it is never recorded as a verdict.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::role_target::{handoff_path, PR_FILE_ENV};
use crate::telemetry::trace::store::CONTEXT_FILE_ENV;
use crate::telemetry::{RoleTickObserved, RoleTickVerdict};

/// `noop_reason` of a spawned Judge tick that claimed no PR and wrote no verdict.
pub const NO_CLAIM_NO_VERDICT: &str = "no_claim_no_verdict";

/// File extension of a verdict handoff.
const EXTENSION: &str = "verdicts";

/// One recorded verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub pr_number: u64,
    pub head_sha: String,
    /// `approved` or `changes_requested`.
    pub verdict: String,
    pub at: DateTime<Utc>,
}

/// The marker vocabulary (`changes-requested`) in the span's snake_case.
#[must_use]
pub fn snake_verdict(verdict: &str) -> Option<&'static str> {
    match verdict {
        "approved" => Some("approved"),
        "changes-requested" | "changes_requested" => Some("changes_requested"),
        _ => None,
    }
}

fn sha_ok(sha: &str) -> bool {
    (7..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Append one entry to `path`. Best effort.
fn append(path: &Path, entry: &Entry) {
    let Ok(mut line) = serde_json::to_string(entry) else {
        return;
    };
    line.push('\n');
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// Record a verified verdict for the launching tick / sweep, whichever gave
/// this process a handoff. A no-op outside both, and for a verdict or head
/// that is not well formed.
pub fn record(pr: u64, head_sha: &str, verdict: &str) {
    let (Some(verdict), true) = (snake_verdict(verdict), sha_ok(head_sha)) else {
        return;
    };
    let entry = Entry {
        pr_number: pr,
        head_sha: head_sha.to_ascii_lowercase(),
        verdict: verdict.to_string(),
        at: Utc::now(),
    };
    for var in [PR_FILE_ENV, CONTEXT_FILE_ENV] {
        if let Some(path) = std::env::var_os(var).filter(|p| !p.is_empty()) {
            append(&PathBuf::from(path).with_extension(EXTENSION), &entry);
        }
    }
}

fn read(path: &Path) -> Vec<Entry> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Read and remove the verdicts tick `execution` of `root` handed back.
pub(super) fn take_tick(root: &Path, execution: &str) -> Vec<Entry> {
    let path = handoff_path(root, execution).with_extension(EXTENSION);
    let entries = read(&path);
    let _ = std::fs::remove_file(&path);
    entries
}

/// The newest verdict recorded for `pr` beside the trace-context file
/// `context` (a sweep phase's handoff).
pub(super) fn for_pr(context: &Path, pr: u64) -> Option<Entry> {
    read(&context.with_extension(EXTENSION))
        .into_iter()
        .rfind(|e| e.pr_number == pr)
}

/// What a spawned Judge tick found and decided, from its prompt/claim PR and
/// the verdicts it handed back (#12029). `found_work` is explicit either way:
/// `true` for a claim, an assigned PR or a verdict, else `false` with the
/// daemon-observed `no_claim_no_verdict`.
#[must_use]
pub fn observation(pr: Option<u64>, entries: &[Entry]) -> RoleTickObserved {
    let Some(newest) = entries.last() else {
        return match pr {
            Some(pr) => RoleTickObserved {
                found_work: Some(true),
                pr_number: Some(pr),
                ..RoleTickObserved::default()
            },
            None => RoleTickObserved {
                found_work: Some(false),
                noop_reason: Some(NO_CLAIM_NO_VERDICT.to_string()),
                ..RoleTickObserved::default()
            },
        };
    };
    RoleTickObserved {
        found_work: Some(true),
        pr_number: Some(newest.pr_number),
        head_sha: Some(newest.head_sha.clone()),
        verdict: Some(newest.verdict.clone()),
        verdicts: Some(
            entries
                .iter()
                .map(|e| RoleTickVerdict {
                    pr_number: e.pr_number,
                    head_sha: e.head_sha.clone(),
                    verdict: e.verdict.clone(),
                })
                .collect(),
        ),
        ..RoleTickObserved::default()
    }
}

/// [`observation`] for a Judge tick; every other role observes nothing here.
#[must_use]
pub(super) fn observation_for(role: &str, pr: Option<u64>, entries: &[Entry]) -> RoleTickObserved {
    if role == "judge" {
        observation(pr, entries)
    } else {
        RoleTickObserved::default()
    }
}

/// A Judge checkpoint's `loom.judge_verdict` / `loom.head_sha` (#12029): the
/// verdict handed back for `pr`, never inferred from the checkpoint phase.
/// With no handoff for this PR they stay absent ("unknown != zero").
pub(super) fn stamp_checkpoint(
    metadata: &mut crate::telemetry::trace::TraceAttributes,
    journal: &crate::telemetry::trace::journal::Journal,
    role: &str,
    pr: Option<u32>,
) {
    let (true, Some(pr)) = (role == "judge", pr) else {
        return;
    };
    if let Some(entry) = for_pr(journal.path(), u64::from(pr)) {
        metadata.insert("loom.judge_verdict".into(), entry.verdict);
        metadata.insert("loom.head_sha".into(), entry.head_sha);
    }
}

/// [`observation`] as `loom.role_attempt` close attributes.
#[must_use]
pub fn span_attributes(o: &RoleTickObserved) -> crate::telemetry::trace::TraceAttributes {
    let mut attrs = crate::telemetry::trace::TraceAttributes::new();
    if let Some(found) = o.found_work {
        attrs.insert("loom.found_work".into(), found.to_string());
    }
    for (key, value) in [
        ("loom.noop_reason", &o.noop_reason),
        ("loom.judge_verdict", &o.verdict),
        ("loom.head_sha", &o.head_sha),
    ] {
        if let Some(value) = value {
            attrs.insert(key.into(), value.clone());
        }
    }
    // Only a verdict names a PR here: the assigned/claimed PR is stamped by
    // the caller, and the newest verdict's PR must win when they differ.
    if let (Some(_), Some(pr)) = (&o.verdict, o.pr_number) {
        attrs.insert(super::role_target::PR_NUMBER.into(), pr.to_string());
    }
    attrs
}

/// Drop the sweep handoff beside `context`.
pub(super) fn remove_for(context: &Path) {
    let _ = std::fs::remove_file(context.with_extension(EXTENSION));
}

#[cfg(test)]
#[path = "verdict_handoff/tests.rs"]
mod tests;
