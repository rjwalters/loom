//! `merge-pr consolidate-reconcile` orchestration (#9689, ADR-0023 §6).
//!
//! Lives in the library (not the CLI) so the whole state machine — not just
//! the pure helpers it calls — runs under test against a stub `gh` and a real
//! git repository: restart idempotency, "an unverified component stays open",
//! and a failed label removal that must not claim a release.
//!
//! Order per component: verify inclusion → status → release → close PR →
//! close declared issues; then the candidate branch, last. Every step
//! re-reads live state before acting, and this verb never merges.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::{
    component_close_body, inclusion_verified, issue_close_body, landing_release_body,
    parse_mapping, status_comment_body, status_present, SEQUENCE_LABEL,
};
use crate::merge_pr::sequence::{fetch_trusted_bodies, parse_live};

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
    pub released: usize,
    /// Components whose `loom:sequenced` label could not be removed. No
    /// release comment was posted for them (the comment would claim a release
    /// that did not happen); a re-run retries.
    pub release_failed: Vec<u32>,
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
        self.release_failed.is_empty()
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
            "Reconciled candidate #{}: {} status(es) posted, {} reservation(s) released, {} \
             component PR(s) closed, {} linked issue(s) closed, branch {branch}",
            self.candidate, self.statuses, self.released, self.closed_prs, self.closed_issues
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
        released: 0,
        release_failed: Vec::new(),
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

        // 3. Status (idempotent by ledger marker).
        let bodies =
            fetch_trusted_bodies(&bin, root, "{owner}/{repo}", *number).unwrap_or_default();
        if !status_present(&bodies, candidate, *number) {
            let body = status_comment_body(*number, candidate, &merge_sha, &mapping.attempt);
            write(gh, root, &["pr", "comment", &n, "--body", &body], "status comment")?;
            report.statuses += 1;
        }

        // 4. Release THIS attempt's reservation — only while the history says
        // it is still in force (a prior run's release tombstone ends it, so a
        // re-run does not release twice). Label first; the comment only after
        // the label is actually off, or it would claim a release that did not
        // happen.
        if let Some(marker) = parse_live(&bodies).filter(|m| m.plan == mapping.attempt) {
            let out = run(gh, root, &["pr", "edit", &n, "--remove-label", SEQUENCE_LABEL])?;
            if out.status.success() {
                let body = landing_release_body(&marker, &mapping.attempt);
                write(gh, root, &["pr", "comment", &n, "--body", &body], "release comment")?;
                report.released += 1;
            } else {
                eprintln!(
                    "consolidate-reconcile: removing {SEQUENCE_LABEL} from #{n} failed — no \
                     release recorded; re-run to retry: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                report.release_failed.push(*number);
            }
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
