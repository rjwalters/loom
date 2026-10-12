//! `loom-daemon pr-latency` — per-segment PR latency, derived live from the
//! forge timeline (Issue #8923).
//!
//! # Why live, and not a query against a rollup
//!
//! Every number here is a difference between two label transitions, and Loom
//! persists **no** label-transition history: the cycle-time rollup (#8665)
//! starts at `sweep.started` and says so explicitly in its own question doc,
//! and `forge_events` is a read-only wake signal. Deriving each segment from
//! `issues/<n>/timeline` per run is therefore the only option that does not
//! first require building a transitions store — see `defaults/docs/pr-latency.md`
//! for that decision in full. `claim_reconciliation` already derives verdict
//! staleness the same way, so this is the established shape, not a new one.
//!
//! # Cost
//!
//! One `gh pr list` plus one `gh api --paginate` per PR. At the default
//! `--limit` that is a few dozen reads: fine on demand, not something to put on
//! a tick. `--advise` narrows the enumeration to open PRs, which is the cheap
//! mode the pre-wave advisory uses.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use loom_daemon::cmd_out::Query;
use loom_daemon::pr_latency::timeline::parse_timeline;
use loom_daemon::pr_latency::{LatencyReport, PrEvent, PrHistory, PrState};
use loom_daemon::script_helpers::gh_query;

use super::pr_latency_render as render;

/// How many PRs to examine by default.
///
/// Sized to the sample #8923 was filed from (the last 40 merges plus ~20 open),
/// which is roughly two days of this fleet's throughput. Large enough that the
/// distributions are not anecdotes, small enough that the run stays under a
/// minute of forge reads.
const DEFAULT_LIMIT: u32 = 60;

/// Default advisory threshold, in hours.
///
/// A day, because that is the boundary at which the hold stops being "a human
/// will get to it" and starts being "nobody knows this is waiting". #8923's
/// measured gated-approval dwell was ~68h median, i.e. this fires well before
/// the observed steady state rather than describing it.
const DEFAULT_THRESHOLD_HOURS: i64 = 24;

#[derive(clap::Args)]
pub(crate) struct PrLatencyArgs {
    /// Emit one JSON document on stdout instead of the human report.
    #[arg(long)]
    pub json: bool,

    /// Advisory mode: the live queue view only, with a warning for anything
    /// past `--threshold-hours`. Always exits 0 and reads only open PRs.
    #[arg(long)]
    pub advise: bool,

    /// Suppress the one-line stdout confirmation when `--advise` finds nothing.
    #[arg(long, short = 'q')]
    pub quiet: bool,

    /// Hours of queue dwell at which `--advise` warns.
    #[arg(long, value_name = "H", default_value_t = DEFAULT_THRESHOLD_HOURS)]
    pub threshold_hours: i64,

    /// Repository to measure, as `owner/name`. Defaults to whatever `gh`
    /// resolves from `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory to run `gh` from. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Maximum number of PRs to examine, most recently created first.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_LIMIT)]
    pub limit: u32,

    /// Report progress to stderr while the timelines are fetched.
    #[arg(long)]
    pub progress: bool,
}

/// One row of `gh pr list --json ...`.
#[derive(Debug, Clone, Deserialize)]
struct PrRow {
    number: u32,
    #[serde(rename = "createdAt")]
    created_at: DateTime<Utc>,
    #[serde(rename = "mergedAt")]
    merged_at: Option<DateTime<Utc>>,
    /// When it closed, merged or not: the ETA stage episodes (#10218) end a
    /// PR closed unmerged here. Same list call, no extra read.
    #[serde(rename = "closedAt", default)]
    closed_at: Option<DateTime<Utc>>,
    state: String,
    #[serde(default)]
    labels: Vec<LabelRef>,
}

#[derive(Debug, Clone, Deserialize)]
struct LabelRef {
    name: String,
}

/// Enumerate `root`'s PRs (`repo`, else whatever `gh` resolves) and fetch
/// each one's timeline, newest first. Shared by `pr-latency` and its
/// callers.
pub(crate) fn fetch_histories(
    root: &Path,
    repo: Option<&str>,
    limit: u32,
    open_only: bool,
    progress: bool,
) -> (Vec<PrHistory>, Option<String>) {
    fetch_histories_matching(root, repo, limit, open_only, None, progress)
}

/// [`fetch_histories`], narrowed by a `gh pr list --search` expression.
///
/// The incremental half of the former fleet refresh (#9343): with
/// `search = "updated:>=<cursor>"` the enumeration covers only PRs that moved
/// since the cached snapshot was built, so the per-PR timeline reads — the
/// expensive part — are paid for a handful of PRs instead of the whole repo.
pub(crate) fn fetch_histories_matching(
    root: &Path,
    repo: Option<&str>,
    limit: u32,
    open_only: bool,
    search: Option<&str>,
    progress: bool,
) -> (Vec<PrHistory>, Option<String>) {
    let (rows, list_error) = list_prs(root, repo, limit, open_only, search);
    let mut histories = Vec::with_capacity(rows.len());
    let total = rows.len();
    for (i, row) in rows.into_iter().enumerate() {
        if progress {
            eprintln!("[pr-latency] {}/{total} #{}", i + 1, row.number);
        }
        histories.push(history_for(&row, repo, root));
    }
    (histories, list_error)
}

impl PrLatencyArgs {
    /// In `--advise` mode this always returns `Ok(())` and exits 0 — the
    /// pre-wave-advisory contract shared with `check-stale-blocked`,
    /// `check-host-sleep`, `check-main-freshness` and
    /// `check-quarantine-stashes`. In report mode a forge failure that left
    /// nothing to measure exits 1, because a caller asking for numbers must not
    /// receive an empty table as if it were an answer.
    pub(crate) fn run(self) -> Result<()> {
        let root = self
            .repo_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let repo = self.repo.as_deref();

        let (histories, list_error) =
            fetch_histories(&root, repo, self.limit, self.advise, self.progress);
        let total = histories.len();

        let report = LatencyReport::build(&histories, Utc::now());
        let threshold = self.threshold_hours.max(0) * 3600;

        if self.advise {
            render::advise(&report, threshold, list_error.as_deref(), self.quiet, self.json);
            return Ok(());
        }

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&render::report_json(&report, list_error.as_deref()))?
            );
        } else {
            print!("{}", render::report_text(&report, list_error.as_deref()));
        }
        if total == 0 && list_error.is_some() {
            std::process::exit(1);
        }
        Ok(())
    }
}

/// Enumerate PRs, newest first. `open_only` narrows to the live queues.
///
/// `--state all` deliberately includes closed-unmerged PRs: they are counted in
/// the census so the sample is honest about what it covers, while
/// [`LatencyReport`] keeps their (absent) merge segment out of the merge
/// distributions.
fn list_prs(
    root: &Path,
    repo: Option<&str>,
    limit: u32,
    open_only: bool,
    search: Option<&str>,
) -> (Vec<PrRow>, Option<String>) {
    let limit = limit.to_string();
    let mut args = vec![
        "pr",
        "list",
        "--state",
        if open_only { "open" } else { "all" },
    ];
    if let Some(r) = repo {
        args.extend(["--repo", r]);
    }
    if let Some(s) = search {
        args.extend(["--search", s]);
    }
    args.extend([
        "--json",
        "number,createdAt,mergedAt,closedAt,state,labels",
        "--limit",
        &limit,
    ]);

    let q: Query<Vec<PrRow>> = gh_query(&args, root, false, |v: &Vec<PrRow>| v.is_empty());
    match q {
        Query::Populated(rows) => (rows, None),
        Query::Empty => (Vec::new(), None),
        Query::Malformed { error, .. } => {
            (Vec::new(), Some(format!("gh pr list returned unreadable JSON: {error}")))
        }
        Query::Failed { status, .. } => (Vec::new(), Some(format!("gh pr list exited {status}"))),
        Query::Unavailable(u) => (Vec::new(), Some(format!("gh pr list could not be run: {u:?}"))),
    }
}

/// Fetch and normalize one PR's timeline into a [`PrHistory`].
///
/// A failed or unreadable timeline yields a history with
/// `timeline_complete = false` rather than an error: the PR's existence and
/// current labels are still facts worth reporting, and the report excludes its
/// segments explicitly instead of silently treating a short log as a fast PR.
fn history_for(row: &PrRow, repo: Option<&str>, root: &Path) -> PrHistory {
    let (events, complete) = fetch_timeline(row.number, repo, root);
    PrHistory::new(
        row.number,
        row.created_at,
        PrState::parse(&row.state),
        row.merged_at,
        row.labels.iter().map(|l| l.name.clone()).collect(),
        events,
        complete,
    )
    .with_closed_at(row.closed_at)
}

fn fetch_timeline(pr: u32, repo: Option<&str>, root: &Path) -> (Vec<PrEvent>, bool) {
    // `gh api` has no `--repo` flag (#8263) — the owner/name goes in the path,
    // and the `{owner}/{repo}` placeholders are resolved by `gh` from the cwd
    // when none was given.
    let path = match repo {
        Some(r) => format!("repos/{r}/issues/{pr}/timeline"),
        None => format!("repos/{{owner}}/{{repo}}/issues/{pr}/timeline"),
    };
    let out = loom_daemon::script_helpers::run_gh(&["api", &path, "--paginate"], root, false);
    let bytes = match &out {
        loom_daemon::cmd_out::CmdOutcome::Ran(o) if o.status.success() => o.stdout.clone(),
        _ => return (Vec::new(), false),
    };
    match parse_timeline(&bytes) {
        Some(events) => (events, true),
        None => (Vec::new(), false),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn pr_rows_decode_the_gh_vocabulary() {
        let rows: Vec<PrRow> = serde_json::from_str(
            r#"[{"number":8531,"createdAt":"2026-09-20T00:00:00Z","mergedAt":null,
                 "state":"OPEN","labels":[{"name":"loom:pr"},{"name":"loom:operator"}]}]"#,
        )
        .unwrap();
        assert_eq!(rows[0].number, 8531);
        assert_eq!(PrState::parse(&rows[0].state), PrState::Open);
        assert_eq!(rows[0].labels.len(), 2);
        assert!(rows[0].merged_at.is_none());
    }
}
