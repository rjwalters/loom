//! `merge-pr consolidate-reconcile` orchestration (#9689, ADR-0023 §6).
//!
//! Lives in the library (not the CLI) so the whole state machine — not just
//! the pure helpers it calls — runs under test against a stub `gh` and a real
//! git repository: restart idempotency, "an unverified component stays open",
//! and a failed label removal that must not claim a release.
//!
//! Order per component: verify inclusion and the live head → status →
//! observe the reservation → close PR → close declared issues; then the
//! candidate branch, last. Every step re-reads live state before acting,
//! and this verb never merges.
//!
//! It never releases a reservation either. ADR-0023 §4 (revised 2026-10-01)
//! makes the ordering pass the ONE releaser on landing: predecessor merged at
//! the recorded head ⇒ `CLEAR` ⇒ `Release`. Step 4 only records whether each
//! hold label is still on; reconciliation neither removes it nor waits for
//! it, and a `loom:sequenced` label on a closed PR is inert.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::{
    component_close_body, inclusion_verified, issue_close_body, parse_mapping, status_comment_body,
    status_present, untouched_open_body, untouched_status_marker, SEQUENCE_LABEL,
};
use crate::merge_pr::sequence::fetch_trusted_bodies;

/// What happened to the candidate branch (step 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchCleanup {
    /// This run deleted it.
    Deleted,
    /// It was already gone — an earlier run (or GitHub's auto-delete) did it.
    /// A completed step, not a failure.
    AlreadyGone,
    /// The delete failed for another reason (auth, network, …).
    Failed(String),
}

/// Classify the `DELETE /git/refs/heads/<branch>` result. GitHub answers a
/// missing ref with HTTP 422 "Reference does not exist" (404 on some API
/// fronts); both mean the step is already complete.
#[must_use]
pub fn branch_cleanup_outcome(success: bool, stderr: &str) -> BranchCleanup {
    if success {
        return BranchCleanup::Deleted;
    }
    let s = stderr.trim();
    if s.contains("Reference does not exist") || s.contains("HTTP 404") {
        BranchCleanup::AlreadyGone
    } else {
        BranchCleanup::Failed(s.to_string())
    }
}

/// The outcome of one reconcile run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub candidate: u32,
    pub statuses: usize,
    /// Components still carrying `loom:sequenced` when this run looked
    /// (observe-only, ADR-0023 §6 step 4): the ordering pass has not released
    /// them yet. Not a failure: the pass releases on a later tick.
    pub holds_pending_release: Vec<u32>,
    /// Components pushed after the candidate landed: `untouched-open` status,
    /// left open with their issues (ADR-0023 §6.2). An end state, not a failure.
    pub untouched_open: Vec<u32>,
    /// Components whose transcript or live head could not be read: nothing
    /// was written to them, and a re-run retries.
    pub unread: Vec<u32>,
    pub closed_prs: usize,
    pub closed_issues: usize,
    /// Components whose pinned head is not an ancestor of the recorded
    /// candidate head — left open, nothing written to them.
    pub unverified: Vec<u32>,
    pub branch: BranchCleanup,
}

impl ReconcileReport {
    /// Every step reached its end state (the verb's exit status).
    #[must_use]
    pub fn complete(&self) -> bool {
        self.unread.is_empty()
            && self.unverified.is_empty()
            && !matches!(self.branch, BranchCleanup::Failed(_))
    }

    /// The one-line stdout summary.
    #[must_use]
    pub fn summary(&self) -> String {
        let branch = match &self.branch {
            BranchCleanup::Deleted => "deleted".to_string(),
            BranchCleanup::AlreadyGone => "already deleted".to_string(),
            BranchCleanup::Failed(e) => format!("delete FAILED ({e})"),
        };
        format!(
            "Reconciled candidate #{}: {} status(es) posted, {} component PR(s) closed, {} \
             linked issue(s) closed, {} left untouched-open, {} hold(s) awaiting the ordering \
             pass's release, branch {branch}",
            self.candidate,
            self.statuses,
            self.closed_prs,
            self.closed_issues,
            self.untouched_open.len(),
            self.holds_pending_release.len()
        )
    }
}

fn run(gh: &Path, root: &Path, args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new(gh);
    cmd.args(args).current_dir(root);
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.output()
        .with_context(|| format!("failed to invoke {}", gh.display()))
}

/// Run a forge WRITE; a failure aborts the run (the next run resumes from
/// live state, so stopping is always safe).
fn write(gh: &Path, root: &Path, args: &[&str], what: &str) -> Result<()> {
    let out = run(gh, root, args)?;
    if !out.status.success() {
        bail!("gh {what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// A `--jq`-reduced read; `None` on failure (the caller treats unknown as
/// "do not act").
fn read(gh: &Path, root: &Path, args: &[&str]) -> Result<Option<String>> {
    let out = run(gh, root, args)?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// Reconcile a MERGED candidate. Never merges; refuses anything else.
pub fn reconcile(gh: &Path, git: &str, root: &Path, candidate: u32) -> Result<ReconcileReport> {
    let bin = gh.to_string_lossy().to_string();
    let cand = candidate.to_string();

    // 0. The candidate's own state.
    let out = run(
        gh,
        root,
        &[
            "pr",
            "view",
            &cand,
            "--json",
            "state,body,headRefName,mergeCommit",
        ],
    )?;
    if !out.status.success() {
        bail!(
            "reading candidate PR #{candidate}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CandidatePr {
        state: String,
        body: String,
        head_ref_name: String,
        merge_commit: Option<MergeCommit>,
    }
    #[derive(Deserialize)]
    struct MergeCommit {
        oid: String,
    }
    let c: CandidatePr = serde_json::from_slice(&out.stdout).context("parse candidate JSON")?;
    if c.state != "MERGED" {
        bail!(
            "candidate #{candidate} is {} — land it first through merge-pr.sh (the canonical \
             path); reconciliation runs only after a verified landing (ADR-0023 §3)",
            c.state
        );
    }
    let Some(merge) = &c.merge_commit else {
        bail!(
            "candidate #{candidate} is MERGED but the forge withheld its merge commit — retry \
             when the API reports it"
        );
    };
    let merge_sha = merge.oid.clone();
    let Some(mapping) = parse_mapping(&c.body) else {
        bail!("candidate #{candidate} carries no consolidation mapping — not a candidate PR");
    };

    let mut report = ReconcileReport {
        candidate,
        statuses: 0,
        holds_pending_release: Vec::new(),
        untouched_open: Vec::new(),
        unread: Vec::new(),
        closed_prs: 0,
        closed_issues: 0,
        unverified: Vec::new(),
        branch: BranchCleanup::Deleted,
    };

    for (number, pinned_head) in &mapping.components {
        // 1-2. Inclusion by ancestry against the RECORDED candidate head.
        if !inclusion_verified(git, root, pinned_head, &mapping.candidate_head) {
            report.unverified.push(*number);
            continue;
        }
        let n = number.to_string();

        // The transcript backs the status ledger. An unreadable one stops this
        // component (nothing written) rather than re-posting blind.
        let Some(bodies) = fetch_trusted_bodies(&bin, root, "{owner}/{repo}", *number) else {
            report.unread.push(*number);
            continue;
        };

        // 2. The source's LIVE head against its pin (ADR-0023 §6.2). A push
        // after landing is past the abort point: the landed pin is on the
        // default branch, the newer commits are not, so the PR is left open
        // with an `untouched-open` status and its issues stay open.
        let Some(live_head) = read(
            gh,
            root,
            &[
                "pr",
                "view",
                &n,
                "--json",
                "headRefOid",
                "--jq",
                ".headRefOid",
            ],
        )?
        else {
            report.unread.push(*number);
            continue;
        };
        if live_head != *pinned_head {
            let marker = untouched_status_marker(candidate, *number);
            if !bodies.iter().any(|b| b.contains(&marker)) {
                let body = untouched_open_body(
                    *number,
                    candidate,
                    &merge_sha,
                    pinned_head,
                    &mapping.attempt,
                );
                write(gh, root, &["pr", "comment", &n, "--body", &body], "status comment")?;
                report.statuses += 1;
            }
            report.untouched_open.push(*number);
            continue;
        }

        // 3. Status (idempotent by ledger marker).
        if !status_present(&bodies, candidate, *number) {
            let body = status_comment_body(*number, candidate, &merge_sha, &mapping.attempt);
            write(gh, root, &["pr", "comment", &n, "--body", &body], "status comment")?;
            report.statuses += 1;
        }

        // 4. Observe the reservation (no write). The ordering pass releases
        // it on `CLEAR`; whether it has yet only goes in the report.
        if read(
            gh,
            root,
            &[
                "pr",
                "view",
                &n,
                "--json",
                "labels",
                "--jq",
                ".labels[].name",
            ],
        )?
        .is_some_and(|labels| labels.lines().any(|l| l.trim() == SEQUENCE_LABEL))
        {
            report.holds_pending_release.push(*number);
        }

        // 5. Close the component PR (idempotent — only an OPEN PR is closed).
        if read(gh, root, &["pr", "view", &n, "--json", "state", "--jq", ".state"])?.as_deref()
            == Some("OPEN")
        {
            let body = component_close_body(candidate, &merge_sha);
            write(gh, root, &["pr", "close", &n, "--comment", &body], "component close")?;
            report.closed_prs += 1;
        }

        // 6. Close the issues THIS component declared, idempotent by state.
        if let Some(pr_body) =
            read(gh, root, &["pr", "view", &n, "--json", "body", "--jq", ".body"])?
        {
            for issue in crate::merge_pr::refs::closing_refs(&pr_body) {
                let i = issue.to_string();
                if read(gh, root, &["issue", "view", &i, "--json", "state", "--jq", ".state"])?
                    .as_deref()
                    == Some("OPEN")
                {
                    let body = issue_close_body(*number, candidate, &merge_sha);
                    write(gh, root, &["issue", "close", &i, "--comment", &body], "issue close")?;
                    report.closed_issues += 1;
                }
            }
        }
    }

    // 7. Branch cleanup, last.
    let out = run(
        gh,
        root,
        &[
            "api",
            "-X",
            "DELETE",
            &format!("repos/{{owner}}/{{repo}}/git/refs/heads/{}", c.head_ref_name),
        ],
    )?;
    report.branch =
        branch_cleanup_outcome(out.status.success(), &String::from_utf8_lossy(&out.stderr));
    Ok(report)
}

#[cfg(test)]
#[path = "reconcile_tests.rs"]
mod tests;
