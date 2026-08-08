//! `loom-daemon stashes list` / `retire` handlers (Issue #5693).
//!
//! Pure local git/`gh` operation — unlike `Quarantine` (in-memory insta-crash
//! pauses in a running daemon), everything this command needs lives in the
//! repo's own `refs/stash` and the forge, so it never touches the daemon's
//! Unix socket and runs on the sync `handle_cli_command` path.

use anyhow::Result;

use loom_daemon::repo_root::resolve_repo_root;
use loom_daemon::stash_retirement::{
    self, DropOutcome, GhIssueStateLookup, PathVerdict, QuarantineStashEntry, RetireVerdict,
    RetirementReport, RETIREMENT_LOG_RELPATH,
};

use crate::StashesAction;

pub(crate) fn handle_stashes_command(action: StashesAction) -> Result<()> {
    match action {
        StashesAction::List {
            workspace,
            issue,
            paths,
            json,
        } => run(&workspace, issue, false, paths, json),
        StashesAction::Retire {
            workspace,
            execute,
            issue,
            paths,
            json,
        } => run(&workspace, issue, execute, paths, json),
    }
}

/// Shared body for `list` (never executes) and `retire` (`execute` from
/// `--execute`) — `list` is exactly `retire` without `--execute`, so both
/// funnel through the same classify-then-optionally-drop path rather than
/// risking two classifiers that could disagree.
fn run(
    workspace: &str,
    issue_filter: Option<u64>,
    execute: bool,
    show_paths: bool,
    json: bool,
) -> Result<()> {
    let repo_root = resolve_repo_root(workspace)?;

    let mut entries = stash_retirement::list_quarantine_stashes(&repo_root)
        .map_err(|e| anyhow::anyhow!("failed to enumerate quarantine stashes: {e}"))?;
    if let Some(issue) = issue_filter {
        entries.retain(|e| e.issue == Some(issue));
    }

    if entries.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("no outstanding loom-quarantine: stashes.");
        }
        return Ok(());
    }

    let mut lookup = GhIssueStateLookup::new(repo_root.clone());
    let reports =
        stash_retirement::plan_and_execute_retirement(&repo_root, &entries, &mut lookup, execute);

    if json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        render_human(&reports, execute, show_paths);
    }

    Ok(())
}

fn render_human(reports: &[RetirementReport], execute: bool, show_paths: bool) {
    let retire_count = reports
        .iter()
        .filter(|r| matches!(r.verdict, RetireVerdict::Retire { .. }))
        .count();
    let keep_count = reports.len() - retire_count;

    println!(
        "{} loom-quarantine: stash(es) classified — {retire_count} retirable, {keep_count} kept.",
        reports.len()
    );
    if !execute && retire_count > 0 {
        println!(
            "(dry run — nothing was dropped. Re-run with `stashes retire --execute` to drop the \
             {retire_count} retirable entr{plural}.)",
            plural = if retire_count == 1 { "y" } else { "ies" }
        );
    }
    println!();

    for report in reports {
        print_entry(report, show_paths);
    }

    if execute && retire_count > 0 {
        println!();
        println!(
            "Every drop was journaled to {RETIREMENT_LOG_RELPATH} with its stash commit sha \
             first; a dropped stash stays recoverable with `git stash apply <sha>` until the \
             object is gc'd."
        );
    }
}

/// How many per-path proof lines to print per stash before eliding. #5690's
/// worst case was a single stash of 1,749 files; dumping all of them would
/// bury the verdicts. `--paths` prints them all.
const PATH_PREVIEW_LIMIT: usize = 10;

fn print_paths(paths: &[(String, PathVerdict)], show_all: bool) {
    let limit = if show_all {
        paths.len()
    } else {
        PATH_PREVIEW_LIMIT
    };
    // Blocking paths first: on a Keep verdict they are the reason, and on a
    // long path list they must never be the ones elided.
    let mut ordered: Vec<&(String, PathVerdict)> = paths.iter().collect();
    ordered.sort_by_key(|(_, v)| v.is_safe());
    for (path, verdict) in ordered.iter().take(limit) {
        println!("            {path}: {}", describe_path_verdict(verdict));
    }
    if paths.len() > limit {
        println!(
            "            … and {} more path(s) (re-run with --paths for the full list)",
            paths.len() - limit
        );
    }
}

fn print_entry(report: &RetirementReport, show_paths: bool) {
    let entry: &QuarantineStashEntry = &report.entry;
    let issue_display = entry
        .issue
        .map(|n| format!("#{n}"))
        .unwrap_or_else(|| "(none)".to_string());
    match &report.verdict {
        RetireVerdict::Retire { reason, paths } => {
            println!(
                "  RETIRE  {} (issue {issue_display}, {}) — {reason}",
                entry.stash_ref, entry.age
            );
            print_paths(paths, show_paths);
        }
        RetireVerdict::Keep { reason, paths } => {
            println!(
                "  KEEP    {} (issue {issue_display}, {}) — {reason}",
                entry.stash_ref, entry.age
            );
            print_paths(paths, show_paths);
        }
    }
    match &report.outcome {
        Some(DropOutcome::Dropped) => {
            println!("            -> dropped (recover: git stash apply {})", entry.commit);
        }
        Some(DropOutcome::AlreadyGone) => println!("            -> already gone (no-op)"),
        Some(DropOutcome::Failed { reason }) => println!("            -> FAILED to drop: {reason}"),
        None => {}
    }
    if let Some(err) = &report.log_error {
        println!("            -> WARNING: could not journal the retirement, drop skipped: {err}");
    }
}

fn describe_path_verdict(verdict: &PathVerdict) -> String {
    match verdict {
        PathVerdict::IdenticalToHead => "identical to HEAD".to_string(),
        PathVerdict::SupersededInHistory { commit } => {
            format!("superseded — identical to {}:<path>", &commit[..commit.len().min(12)])
        }
        PathVerdict::IgnorableDirt => "installer-managed / regenerable".to_string(),
        PathVerdict::GeneratedArtifact => "machine-generated artifact".to_string(),
        PathVerdict::NotProvenRecoverable => "NOT provably recoverable".to_string(),
    }
}
