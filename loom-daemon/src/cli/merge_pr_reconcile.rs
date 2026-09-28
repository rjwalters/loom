//! `loom-daemon merge-pr reconcile-plan` and `merge-pr reconcile-child` (#3747
//! item 1, a slice of the merge-pr port #8191): the two decisions behind
//! `merge-pr.sh`'s post-merge stacked-child reconciliation.
//!
//! # Protocol
//!
//! Both verbs take their large, forge-supplied input on **stdin** (a rollup and
//! a label list respectively — untrusted text of unbounded length, which is the
//! wrong thing to put in an argument vector) and answer with sentinel-led lines.
//!
//! ## `reconcile-plan --parent-branch <branch>`
//!
//! stdin is the `[{number, headRefName}]` children rollup. stdout is exactly one
//! of:
//!
//! ```text
//! LOOM-RECONCILE-PLAN NOT-STACKED
//! LOOM-RECONCILE-PLAN UNREADABLE <detail>
//! ```
//!
//! …or a `COUNT` line followed by one line per rollup element, in the rollup's
//! own order:
//!
//! ```text
//! LOOM-RECONCILE-PLAN COUNT <n>
//! LOOM-RECONCILE-PLAN CHILD <pr><TAB><branch><TAB><issue-or-empty>
//! LOOM-RECONCILE-PLAN MALFORMED <index><TAB><detail>
//! ```
//!
//! `COUNT` is the number of lines that follow, malformed ones included, so the
//! shell can tell a truncated read from a short rollup. `CHILD`'s third field is
//! empty when the child branch is not `feature/issue-<N>` — the case the retired
//! shell expressed as an empty `$child_issue` and treated as "no claim to race".
//!
//! ## `reconcile-child --child-pr <n> --parent-branch <branch> [--child-issue <n>] [--now <ts>]`
//!
//! stdin is the child issue's label names, one per line, as the shell's uncached
//! `gh api repos/<nwo>/issues/<n>` read produces them. stdout is:
//!
//! ```text
//! LOOM-RECONCILE-CHILD reconcile
//! ```
//!
//! or, for the defer route, the verdict line, a `LOOM-RECONCILE-COMMENT` marker,
//! and then the comment body verbatim to EOF:
//!
//! ```text
//! LOOM-RECONCILE-CHILD defer
//! LOOM-RECONCILE-COMMENT
//! ## Stacked parent merged — reconciliation deferred
//! …
//! ```
//!
//! The body is last and unterminated on purpose: the shell takes everything
//! after the marker as-is, so the text cannot be reshaped by a line-oriented
//! read. `--now` exists only so the differential harness can pin the
//! attribution timestamp; omitted, the verb reads the clock itself, which is
//! what removes the shell's own `date -u` call.
//!
//! # Exit code, and why fail-OPEN is right here
//!
//! **0 whenever a decision was printed** — including `NOT-STACKED`, `UNREADABLE`
//! and `MALFORMED`, all of which are real answers. 2 only when stdin could not
//! be read at all, which is not an answer.
//!
//! `merge-pr.sh` accepts only exit 0 plus the sentinel; anything else (no
//! binary, one predating these verbs — clap exits 2 — a usage error, silence)
//! makes it **warn and skip auto-reconciliation for this pass**, which is
//! byte-for-byte the disposition its pre-existing "reconcile-stack.sh not found
//! or not executable" skip already has.
//!
//! That degradation is correct here and would be wrong for the merge gates. This
//! pass runs *after* the merge has already happened and returns 0
//! unconditionally; it cannot make a merge wrong, only leave a child PR needing
//! the manual `reconcile-stack.sh` invocation that every message on both routes
//! already prints. Failing closed would instead stop merging on any host whose
//! daemon lags a release — a cleanup step holding the pipeline. Same argument,
//! same conclusion, as this guard's pre-merge sibling
//! `merge_pr::stacked_children`; neither raises the `requires-daemon: merge-pr`
//! floor, and `merge-pr.sh`'s floor block says so.
//!
//! Crucially the skip is reachable only through the **absence** of a sentinel.
//! Silence is never a route, so a daemon that dies mid-write cannot be read as
//! `reconcile` — which is the route that force-pushes.

use anyhow::Result;
use loom_daemon::merge_pr::reconcile::{child_route, defer_comment, plan, Plan, Row};
use std::io::{Read, Write};

/// Read all of stdin as bytes, or exit 2.
///
/// `read_to_end`, not `read_to_string`: neither input is guaranteed valid UTF-8
/// and the `jq`/`grep` pipelines this replaces never required it to be.
fn stdin_bytes(verb: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    if std::io::stdin().read_to_end(&mut buf).is_err() {
        eprintln!("merge-pr {verb}: could not read stdin");
        std::process::exit(2);
    }
    buf
}

#[derive(clap::Args)]
pub(crate) struct ReconcilePlanArgs {
    /// The just-merged PARENT PR's head branch. Only a `feature/issue-<N>`
    /// branch can have stacked children by Loom's convention.
    #[arg(long)]
    parent_branch: String,
}

impl ReconcilePlanArgs {
    pub(crate) fn run(self) -> Result<()> {
        let rollup = stdin_bytes("reconcile-plan");
        let mut out = std::io::stdout().lock();
        match plan(&self.parent_branch, &rollup) {
            Plan::NotStacked => writeln!(out, "LOOM-RECONCILE-PLAN NOT-STACKED")?,
            Plan::Unreadable(detail) => {
                writeln!(out, "LOOM-RECONCILE-PLAN UNREADABLE {detail}")?;
            }
            Plan::Rows(rows) => {
                writeln!(out, "LOOM-RECONCILE-PLAN COUNT {}", rows.len())?;
                for row in rows {
                    match row {
                        Row::Child { pr, branch, issue } => writeln!(
                            out,
                            "LOOM-RECONCILE-PLAN CHILD {pr}\t{branch}\t{}",
                            issue.unwrap_or_default()
                        )?,
                        Row::Malformed { index, detail } => {
                            writeln!(out, "LOOM-RECONCILE-PLAN MALFORMED {index}\t{detail}")?;
                        }
                    }
                }
            }
        }
        out.flush()?;
        Ok(())
    }
}

#[derive(clap::Args)]
pub(crate) struct ReconcileChildArgs {
    /// The child PR number, as the plan reported it.
    #[arg(long)]
    child_pr: String,

    /// The just-merged parent branch, named in the deferral comment.
    #[arg(long)]
    parent_branch: String,

    /// The child's issue number, as the plan reported it. Omitted (or empty)
    /// means the child branch is not `feature/issue-<N>`, so there is no
    /// `loom:building` claim to race.
    #[arg(long)]
    child_issue: Option<String>,

    /// Attribution timestamp for the deferral comment, in the shell's
    /// `date -u +%Y-%m-%dT%H:%M:%SZ` shape. Defaults to now (UTC); supplied
    /// only by the differential harness, which needs it pinned.
    #[arg(long)]
    now: Option<String>,
}

impl ReconcileChildArgs {
    pub(crate) fn run(self) -> Result<()> {
        let labels = stdin_bytes("reconcile-child");
        // An explicitly empty --child-issue is the same as omitting it: the
        // shell passes `${child_issue:+--child-issue "$child_issue"}`, but a
        // hand invocation may well pass `--child-issue ""`.
        let issue = self.child_issue.filter(|s| !s.is_empty());
        let decision = child_route(issue.as_deref(), &labels);
        let mut out = std::io::stdout().lock();
        writeln!(out, "LOOM-RECONCILE-CHILD {}", decision.route.token())?;
        if decision.route == loom_daemon::merge_pr::reconcile::Route::Defer {
            let timestamp = self
                .now
                .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
            writeln!(out, "LOOM-RECONCILE-COMMENT")?;
            // `write!`, not `writeln!`: the retired `comment="…"` had no
            // trailing newline and the shell posts what it is given.
            write!(
                out,
                "{}",
                defer_comment(
                    issue.as_deref().unwrap_or_default(),
                    &self.child_pr,
                    &self.parent_branch,
                    &timestamp,
                )
            )?;
        }
        out.flush()?;
        Ok(())
    }
}
