//! `merge-pr.sh`'s post-merge stacked-child reconciliation, as one orchestrator
//! (#8191, slice 3 of `docs/migration/merge-pr-orchestrator-port.md`).
//!
//! The shell ran `reconcile-plan` (twice at most), then per child a fresh label
//! read, `reconcile-child`, and either a deferral comment or `reconcile-stack.sh`.
//! Here the same decisions ([`super::reconcile`]) are called in-process in the
//! same order; the shell keeps a wrapper that replays the output.
//!
//! # Output
//!
//! `LEVEL<TAB>message` per line (`INFO`, `WARNING`, `SUCCESS`), ended by
//! [`DONE`]. Exit 0 without the terminator is a stale or muted binary, never
//! "ran and had nothing to say".
//!
//! # Fail direction
//!
//! Best-effort, never fails the merge, as before. Anything unanswered (an
//! unreadable plan, an unreadable child) skips that work and names the manual
//! `reconcile-stack.sh` command; nothing is rebased on a guess. `reconcile-stack.sh`
//! itself stays a script (it is already a thin port, #8583) and is only invoked.

use std::path::Path;
use std::process::Command;

use super::partial_comment::now_timestamp;
use super::post_merge_bookkeeping::Record;
use super::reconcile::{
    child_route, defer_comment, plan, read_lease_state, route_with_lease, Plan, Route, Row,
};

/// Last stdout line of a finished run.
pub const DONE: &str = "LOOM-STACKED-RECONCILE DONE";

/// What the pass knows without asking the forge.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub repo: String,
    /// The merged parent's head branch (`$PR_BRANCH`).
    pub parent_branch: String,
    /// `$STACKED_CHILDREN_JSON`, the pre-merge snapshot (#8010 item 2), if any.
    pub snapshot: Option<String>,
    /// Directory holding `reconcile-stack.sh`.
    pub script_dir: String,
    /// The shell's roll-this-host remediation text, appended to skip warnings.
    pub roll_hint: String,
}

/// Everything the pass does to the outside world.
pub trait Host {
    /// `gh pr list --repo <nwo> --base <parent> --state open --json number,headRefName`,
    /// `[]` on failure.
    fn list_children(&self, repo: &str, parent: &str) -> String;
    /// Names of the child issue's labels, one per line, uncached; empty on failure.
    fn issue_labels(&self, repo: &str, issue: &str) -> String;
    /// Whether a live Builder lease is held on `issue` (see [`read_lease_state`]).
    fn lease(&self, issue: &str) -> super::reconcile::LeaseState;
    /// Post a comment on a PR through the rate-limit-safe writer.
    fn comment(&self, repo: &str, pr: &str, body: &str) -> bool;
    /// Is `reconcile-stack.sh` runnable in `dir`?
    fn script_runnable(&self, dir: &str) -> bool;
    /// Run `reconcile-stack.sh <pr> <parent>`; true on exit 0.
    fn reconcile_stack(&self, dir: &str, pr: &str, parent: &str) -> bool;
    fn timestamp(&self) -> String;
}

fn rec(level: &str, text: impl Into<String>) -> Record {
    (level.to_string(), text.into())
}

fn with_hint(msg: String, hint: &str) -> String {
    if hint.is_empty() {
        msg
    } else {
        format!("{msg} {hint}")
    }
}

/// Run the whole pass.
pub fn run(inputs: &Inputs, host: &dyn Host) -> Vec<Record> {
    let mut out = Vec::new();
    let parent = inputs.parent_branch.as_str();
    let hint = inputs.roll_hint.as_str();
    // At most two plan calls: `gate` (parent-branch gate, fed the snapshot or
    // `[]`), then `live` (a forge query) only for a stacked parent without one.
    let mut children = inputs.snapshot.clone().filter(|s| !s.is_empty());
    let mut rows;
    let mut pass = 0;
    loop {
        pass += 1;
        match plan(parent, children.as_deref().unwrap_or("[]").as_bytes()) {
            Plan::NotStacked => return out,
            Plan::Unreadable(detail) => {
                out.push(rec(
                    "WARNING",
                    with_hint(format!("Stacked reconcile: skipping auto-reconciliation for '{parent}' — 'loom-daemon merge-pr reconcile-plan' exited 0 without a plan (it said: LOOM-RECONCILE-PLAN UNREADABLE {detail}). The parent merge is unaffected; reconcile any stacked child by hand: ./.loom/scripts/reconcile-stack.sh <child-pr> {parent}"), hint),
                ));
                return out;
            }
            Plan::Rows(r) => rows = r,
        }
        if children.is_some() || pass >= 2 {
            break;
        }
        let live = host.list_children(&inputs.repo, parent);
        children = Some(if live.trim().is_empty() {
            "[]".into()
        } else {
            live
        });
    }
    if rows.is_empty() {
        return out;
    }
    out.push(rec(
        "INFO",
        format!("Stacked reconcile: found {} open child PR(s) based on '{parent}'", rows.len()),
    ));
    if !host.script_runnable(&inputs.script_dir) {
        out.push(rec(
            "WARNING",
            format!("Stacked reconcile: reconcile-stack.sh not found or not executable at {} — skipping auto-reconciliation", inputs.script_dir),
        ));
        return out;
    }
    for row in rows.drain(..) {
        match row {
            Row::Child { pr, branch: _, issue } => {
                if pr.is_empty() {
                    continue;
                }
                one_child(inputs, host, &pr, issue.as_deref(), &mut out);
            }
            Row::Malformed { index, detail } => out.push(rec(
                "WARNING",
                format!("Stacked reconcile: ignoring an unusable child row from the forge — {index}\t{detail}"),
            )),
        }
    }
    out
}

fn one_child(
    inputs: &Inputs,
    host: &dyn Host,
    pr: &str,
    issue: Option<&str>,
    out: &mut Vec<Record>,
) {
    let parent = inputs.parent_branch.as_str();
    let labels = issue
        .map(|i| host.issue_labels(&inputs.repo, i))
        .unwrap_or_default();
    let mut decision = child_route(issue, labels.as_bytes());
    if decision.route == Route::Defer {
        if let Some(i) = issue {
            decision = route_with_lease(decision, &host.lease(i));
        }
    }
    let n = issue.unwrap_or_default();
    match decision.route {
        Route::Defer => {
            out.push(rec("INFO", format!("Stacked reconcile: child PR #{pr} (issue #{n}) is still loom:building — deferring auto-rebase to avoid racing a live Builder")));
            let body = defer_comment(n, pr, parent, &host.timestamp());
            if !host.comment(&inputs.repo, pr, &body) {
                out.push(rec(
                    "WARNING",
                    format!("Could not post deferred-reconciliation comment on PR #{pr}"),
                ));
            }
        }
        Route::Reconcile => {
            out.push(rec("INFO", format!("Stacked reconcile: parent '{parent}' merged; reconciling child PR #{pr} onto the default branch")));
            if host.reconcile_stack(&inputs.script_dir, pr, parent) {
                out.push(rec(
                    "SUCCESS",
                    format!("Stacked reconcile: child PR #{pr} reconciled onto the default branch"),
                ));
            } else {
                out.push(rec("WARNING", format!("Stacked reconcile: reconcile-stack.sh failed for child PR #{pr} (rebase conflict, rejected force-with-lease push, or retarget failure). The parent merge is unaffected — reconcile manually: ./.loom/scripts/reconcile-stack.sh {pr} {parent}")));
            }
        }
    }
}

/// Plain uncached `gh`, exactly as the retired shell called it (not the
/// gh-cached `$GH`, so a stale view cannot mask a live re-claim). `None` on any
/// failure; `repo` is unused beyond documentation of the target.
fn plain_gh(args: &[&str]) -> Option<String> {
    let o = Command::new("gh")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The live forge and scripts.
pub struct SystemHost {
    /// Path to `lib/forge-helpers.sh`.
    pub forge_helpers: String,
}

impl Host for SystemHost {
    fn list_children(&self, repo: &str, parent: &str) -> String {
        plain_gh(&[
            "pr",
            "list",
            "--repo",
            repo,
            "--base",
            parent,
            "--state",
            "open",
            "--json",
            "number,headRefName",
        ])
        .unwrap_or_else(|| "[]".into())
    }

    fn issue_labels(&self, repo: &str, issue: &str) -> String {
        let path = format!("repos/{repo}/issues/{issue}");
        let Some(raw) = plain_gh(&["api", &path]) else {
            return String::new();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return String::new();
        };
        v.get("labels")
            .and_then(|l| l.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    fn lease(&self, issue: &str) -> super::reconcile::LeaseState {
        match issue.parse::<u32>() {
            Ok(n) => {
                let root = std::env::current_dir().unwrap_or_else(|_| ".".into());
                read_lease_state(&root, n)
            }
            Err(_) => super::reconcile::LeaseState::Unknown,
        }
    }

    fn comment(&self, repo: &str, pr: &str, body: &str) -> bool {
        Command::new("bash")
            .arg("-c")
            .arg(r#"source "$1" || exit 127; shift; "$@""#)
            .arg("_")
            .arg(&self.forge_helpers)
            .arg("forge_gh_comment_rl_safe")
            .args([repo, pr, body])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn script_runnable(&self, dir: &str) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(Path::new(dir).join("reconcile-stack.sh"))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }

    fn reconcile_stack(&self, dir: &str, pr: &str, parent: &str) -> bool {
        // The script's own output is diagnostics: keep stdout clean for the
        // record protocol by sending it to stderr.
        match Command::new(Path::new(dir).join("reconcile-stack.sh"))
            .args([pr, parent])
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(o) => {
                eprint!("{}", String::from_utf8_lossy(&o.stdout));
                eprint!("{}", String::from_utf8_lossy(&o.stderr));
                o.status.success()
            }
            Err(_) => false,
        }
    }

    fn timestamp(&self) -> String {
        now_timestamp()
    }
}

/// Print a finished run in the wrapper's line protocol.
pub fn print_records(records: &[Record]) {
    super::post_merge_bookkeeping::print_records_with(records, DONE);
}

#[cfg(test)]
mod tests;
