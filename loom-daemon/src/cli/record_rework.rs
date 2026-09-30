//! `loom-daemon record-rework` (Issue #9444): mark one in-sweep rework event,
//! so the sweep's terminal `sweep.outcome` can carry it.
//!
//! `rework_events` shipped with a reader and no writer. Nothing appended to
//! `.loom/logs/sweep-rework-events.jsonl`, so the field could only ever be
//! absent and every rollup over it — SF5's rework columns, `issue_effort`'s
//! substantive/environmental split — reported a fleet in which no work is ever
//! re-done. This is that writer.
//!
//! **Why a subcommand rather than three lines of shell.** The first caller is
//! `merge-pr.sh`'s stale-base handling, and that script is (a) frozen by the
//! file-size ratchet and (b) `contract`-category shell whose logic the
//! shell-language policy points into `loom-daemon`. A subcommand is also what
//! lets the doctor and CI-fix paths mark an event later without a second copy
//! of the marker format. The format itself lives in
//! [`loom_daemon::rework_events`], beside the reader's delegation to it.
//!
//! **Always exits 0.** A marker is telemetry attached to an operation that
//! matters — a merge, a doctor claim. Refusing to record must never refuse the
//! operation, so an unwritable log directory, a workspace root that cannot be
//! resolved, or a branch with no issue in its name all print a reason and exit
//! 0. The one thing it does not do is *guess*: no issue number means no
//! marker, never a marker charged to the wrong issue.
//!
//! Callers that want to assert a marker landed read stdout, which is one JSON
//! object: `{"recorded":1,"kind":…,"issue":…,"path":…}` or
//! `{"recorded":0,"skipped":…}` / `{"recorded":0,"error":…}`.

use std::path::PathBuf;

use loom_daemon::rework_events::{self, Marker};
use loom_daemon::worktree_ops::naming::issue_from_branch;

#[derive(clap::Args)]
pub(crate) struct RecordReworkArgs {
    /// What kind of rework happened.
    #[arg(long, value_parser = rework_events::KINDS.to_vec())]
    kind: String,

    /// The issue whose sweep performed it. Takes precedence over `--branch`.
    #[arg(long)]
    issue: Option<u32>,

    /// Derive the issue from a `feature/issue-<N>` branch name. Ignored when
    /// `--issue` is given; a branch that does not fit the convention (a fork
    /// PR, an ad-hoc name) records nothing rather than guessing.
    #[arg(long)]
    branch: Option<String>,

    /// The workspace root whose `.loom/logs/` holds the marker file. Defaults
    /// to the repository root enclosing the working directory — the same root
    /// the outcome journal reads markers from, so a call made inside a
    /// worktree still lands in the shared file.
    #[arg(long)]
    repo_root: Option<PathBuf>,

    /// Short free text: why the rework happened. Truncated to
    /// `rework_events::MAX_REASON_CHARS`.
    #[arg(long)]
    reason: Option<String>,

    /// Override the kind's default substantive/environmental classification.
    /// Reserved for a caller that genuinely knows better than the table; the
    /// table (normative in `telemetry-schema.md`) is the default for a reason.
    #[arg(long, value_parser = rework_events::CLASSIFICATIONS.to_vec())]
    classification: Option<String>,

    /// How long the rework took, when the caller measured it. Omit rather than
    /// pass 0 — an absent duration is counted as an *open* event, a zero is a
    /// measurement claiming the rework was free.
    #[arg(long)]
    duration_sec: Option<i64>,
}

impl RecordReworkArgs {
    /// Never fails: every refusal is reported on stdout and exits 0.
    pub(crate) fn run(self) -> anyhow::Result<()> {
        let summary = self.record();
        println!("{summary}");
        Ok(())
    }

    fn record(self) -> serde_json::Value {
        let Some(issue) = self
            .issue
            .or_else(|| self.branch.as_deref().and_then(issue_from_branch))
        else {
            return serde_json::json!({
                "recorded": 0,
                "skipped": "no issue: pass --issue, or a --branch matching feature/issue-<N>",
            });
        };

        let workspace_root = match self.repo_root {
            Some(root) => Some(root),
            None => std::env::current_dir()
                .ok()
                .and_then(|cwd| loom_daemon::repo_root::find_repo_root(&cwd)),
        };
        let Some(workspace_root) = workspace_root else {
            return serde_json::json!({
                "recorded": 0,
                "skipped": "no workspace root: run inside a Loom repository or pass --repo-root",
            });
        };

        let marker = Marker {
            issue,
            kind: &self.kind,
            reason: self.reason.as_deref(),
            classification: self.classification.as_deref(),
            duration_sec: self.duration_sec,
        };
        match rework_events::append(&workspace_root, &marker) {
            Ok(path) => serde_json::json!({
                "recorded": 1,
                "kind": self.kind,
                "issue": issue,
                "classification": self
                    .classification
                    .clone()
                    .unwrap_or_else(|| rework_events::default_classification(&self.kind).to_string()),
                "path": path.display().to_string(),
            }),
            Err(error) => serde_json::json!({
                "recorded": 0,
                "error": error.to_string(),
            }),
        }
    }
}
