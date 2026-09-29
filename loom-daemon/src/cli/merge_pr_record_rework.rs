//! `merge-pr record-rework` — the first writer for Issue #9444's rework-event
//! marker protocol. The merge path is where in-sweep rework is performed and
//! observed (a base-sync before a merge retry, a conflict refusal a Doctor
//! turn must resolve), and this verb is how it marks those events for the
//! terminal sweep outcome to sample into `rework_events`.
//!
//! Deliberately FAIL-OPEN, end to end — telemetry must never block a merge,
//! and the calling shell isolates the whole invocation with `|| true` anyway:
//!
//! * exit 0 = recorded, or deliberately skipped (this host's sweep-outcome
//!   journal does not link the PR to an issue — a merge run outside a sweep's
//!   lifecycle, or a PR opened before the journal existed);
//! * exit 2 = misuse (an unknown `--kind`; recording a typo'd kind would read
//!   back as a plausible-but-wrong `environmental` later);
//! * exit 4 = the marker write itself failed, with the reason on stderr.
//!
//! No forge round trip: the issue is resolved from this host's own
//! sweep-outcome telemetry journal (the sweep that opened the PR recorded
//! `repo` + `pr_number` on it), or supplied directly with `--issue`. See
//! `sweep_registry::outcome_journal::rework` for the protocol and the reader.

use anyhow::Result;
use loom_daemon::repo_root::resolve_repo_root;

#[derive(clap::Args)]
pub(crate) struct RecordReworkArgs {
    /// The rework kind — the protocol's closed vocabulary: rebase,
    /// merge_conflict, ci_rerun, rejudge.
    #[arg(long)]
    kind: String,

    /// The PR whose sweep performed (or must now perform) the rework.
    #[arg(long)]
    pr: u32,

    /// The repo slug (`owner/name`) the PR belongs to.
    #[arg(long)]
    repo: String,

    /// The workspace root whose `.loom/logs/` carries the marker file.
    #[arg(long, value_name = "PATH", default_value = ".")]
    workspace: String,

    /// Supply the issue directly instead of resolving it from the journal.
    #[arg(long)]
    issue: Option<u32>,

    /// Human-readable reason carried on the marker (e.g. the classifier's
    /// own reason string).
    #[arg(long)]
    reason: Option<String>,

    /// Measured duration of the rework, when the performing path knows it.
    /// Absent = an "open" event: counted by the effort queries, never charged
    /// fabricated seconds.
    #[arg(long)]
    duration_sec: Option<i64>,
}

impl RecordReworkArgs {
    pub(crate) fn run(self) -> Result<()> {
        use loom_daemon::sweep_registry::{
            append_rework_event, issue_for_pr_from_journal, known_kind, ReworkMarker,
        };

        if !known_kind(&self.kind) {
            eprintln!(
                "merge-pr record-rework: unknown --kind '{}'; the protocol \
                 vocabulary is rebase, merge_conflict, ci_rerun, rejudge",
                self.kind
            );
            std::process::exit(2);
        }
        let root = resolve_repo_root(&self.workspace)?;
        let issue = match self.issue {
            Some(issue) => Some(issue),
            None => issue_for_pr_from_journal(&root, &self.repo, self.pr),
        };
        let Some(issue) = issue else {
            eprintln!(
                "merge-pr record-rework: no sweep-outcome telemetry record on \
                 this host links PR #{} in {} to an issue; skipping the marker \
                 (best-effort by contract)",
                self.pr, self.repo
            );
            return Ok(());
        };
        let marker = ReworkMarker {
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            issue,
            kind: self.kind,
            reason: self.reason,
            duration_sec: self.duration_sec,
        };
        if let Err(error) = append_rework_event(&root, &marker) {
            eprintln!("merge-pr record-rework: could not write the marker: {error:#}");
            std::process::exit(4);
        }
        eprintln!("merge-pr record-rework: recorded {} for issue #{issue}", marker.kind);
        Ok(())
    }
}
