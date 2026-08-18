//! `loom-daemon clean` / `cleanup` / `recover-orphans` handlers (Issue
//! #4712 — split out of `main.rs`). Native ports of `loom-clean` /
//! `loom-cleanup` / `loom-recover-orphans` (Issue #4272).

use anyhow::Result;

use loom_daemon::worktree_ops::clean;

use crate::CleanupAction;

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_clean_command(
    workspace: &str,
    dry_run: bool,
    deep: bool,
    force: bool,
    safe: bool,
    grace_period: i64,
    worktrees_only: bool,
    branches_only: bool,
    tmux_only: bool,
    daemon: bool,
    aggressive: bool,
    aggressive_min_age: u64,
) -> Result<()> {
    use loom_daemon::worktree_ops::aggressive as agg;
    use loom_daemon::worktree_ops::repo;

    let repo_root = repo::resolve_repo_root(workspace)?;

    if daemon {
        println!();
        println!("========================================");
        println!("  Loom Crash Recovery");
        if dry_run {
            println!("  (DRY RUN MODE)");
        }
        println!("========================================");
        println!();
        clean::clean_daemon_crash_state(&repo_root, dry_run);
        if dry_run {
            println!("Dry run complete - no changes made");
        } else {
            println!("Crash recovery complete!");
        }
        println!();
        return Ok(());
    }

    if aggressive {
        println!();
        println!("========================================");
        println!("  Loom Aggressive Worktree Cleanup");
        if dry_run {
            println!("  (DRY RUN MODE)");
        }
        println!("========================================");
        println!();
        eprintln!(
            "Aggressive mode overrides .loom-in-use markers and process-table guards. Respects \
             open PRs, active shepherds, the .loom-managed sentinel, uncommitted changes, \
             reachability from origin/main, and (issue #5950) issue-open state: a worktree \
             whose issue is not CLOSED is kept unless its work is already landed (HEAD on \
             origin/main, or a merged PR) and its working tree is clean."
        );
        if safe {
            eprintln!(
                "--safe: merged-PR-only mode. --force still overrides uncommitted changes, but \
                 not the \"HEAD unreachable — would lose work\" skip (issue #5735) — a worktree \
                 with no merged PR backing it is kept even with --force."
            );
        }
        println!();

        // Issue #5736: `--aggressive` is the most destructive combination
        // (removes worktrees AND branches) — it must pass through the same
        // confirmation gate every other destructive mode uses, so a
        // non-interactive/piped invocation (closed stdin, no TTY) aborts
        // instead of proceeding with zero prompt. `--dry-run` and `--force`
        // keep their existing bypasses (mirrors `run_clean`).
        if !clean::confirm_destructive_action(dry_run, force) {
            println!("Cleanup cancelled");
            println!();
            return Ok(());
        }

        let stats = agg::clean_aggressive(&repo_root, dry_run, force, safe, aggressive_min_age);
        agg::print_aggressive_summary(&stats, dry_run);
        println!("{}", clean::completion_line("Aggressive cleanup", dry_run, stats.errors));
        if dry_run {
            println!("Run without --dry-run to perform cleanup");
        }
        println!();
        std::process::exit(clean::exit_code(stats.errors));
    }

    // Issue #6127: state `--deep`'s blast radius at the point of use, not only
    // in `--help`. The scheduled fleet-clean job runs `--deep --safe -y`, so
    // this line is what an operator reading that job's log sees — and the
    // `--safe`-does-not-narrow-this fact was previously inferable only from
    // #4890's discussion of worktrees and branches.
    if deep {
        eprintln!(
            "--deep removes the primary checkout's build artifacts (target/, node_modules/) in \
             full, including any service binary built there. --safe does NOT narrow this — it \
             gates worktrees and branches only (issue #4890). A build-artifact directory that \
             is currently backing a running program is detected and skipped whole; a service \
             that is stopped when this runs is not protected, so never point a launchd/systemd \
             unit at a path under target/."
        );
        println!();
    }

    let opts = clean::CleanOptions {
        dry_run,
        deep,
        force,
        safe,
        grace_period_secs: grace_period,
        worktrees_only,
        branches_only,
        tmux_only,
        // The interactive CLI keeps its historical behavior: an operator who
        // typed the command is the authority on a sentinel-less worktree. Only
        // the unattended reaper (#4876) requires the `.loom-managed` sentinel.
        require_managed_sentinel: false,
    };
    let exit_code = clean::run_clean(&repo_root, &opts);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

pub(crate) fn handle_cleanup_command(action: CleanupAction) -> Result<()> {
    use loom_daemon::worktree_ops::{logs, repo};
    match action {
        CleanupAction::Logs {
            workspace,
            dry_run,
            prune_only,
            retention_days,
        } => {
            let repo_root = repo::resolve_repo_root(&workspace)?;
            let rc = logs::handle_logs(&repo_root, dry_run, prune_only, retention_days);
            if rc != 0 {
                std::process::exit(rc);
            }
            Ok(())
        }
    }
}

pub(crate) fn handle_recover_orphans_command(
    workspace: &str,
    recover: bool,
    json: bool,
    verbose: bool,
) -> Result<()> {
    use loom_daemon::worktree_ops::{orphan_recovery as orphans, repo};

    let repo_root = repo::resolve_repo_root(workspace)?;

    if !json {
        println!("Orphaned Spawn-Loop Task Detection & Recovery");
        if !recover {
            println!("DRY RUN - No changes will be made");
            println!("Use --recover to actually perform recovery");
        }
    }

    let result = orphans::run_orphan_recovery(&repo_root, recover, verbose);

    if json {
        println!("{}", orphans::format_result_json(&result));
    } else {
        println!("{}", orphans::format_result_human(&result));
    }

    // A failed dependency (e.g. the `gh issue list --label loom:building`
    // query) means the claims could not be enumerated at all. That is not a
    // clean bill of health, so it must never exit 0 — an operator or watch
    // loop checking for stranded claims would read success as "none stranded"
    // (issue #5140). Distinct exit code from the "orphans found" 2 below.
    if result.assessment_failed() {
        eprintln!(
            "recover-orphans could not assess orphaned tasks (see errors above); \
             exiting {EXIT_ASSESSMENT_FAILED} rather than reporting a false all-clear"
        );
        std::process::exit(EXIT_ASSESSMENT_FAILED);
    }

    // Issue #6392: exit 2 is a REPORT ("orphans found"), not a crash — but a
    // caller that only inspects stderr for a failure reason (e.g. sweep.md's
    // capability pre-probe, which quotes the first stderr line on any
    // non-zero exit) previously saw nothing here at all, so it fell back to
    // whatever the binary-resolution wrapper had already printed to stderr
    // (a misleading success trace — see recover-orphaned-shepherds.sh). Make
    // this exit self-explanatory on stderr too, independent of stdout's
    // human/JSON report.
    if !result.orphaned.is_empty() && !recover {
        eprintln!("{}", orphans::format_dry_run_orphans_found_stderr(result.orphaned.len()));
        std::process::exit(2);
    }
    Ok(())
}

/// `recover-orphans` exit code for "the assessment itself failed" — distinct
/// from `2` ("orphans found in dry-run mode") and from `0` ("assessed, nothing
/// orphaned"). Issue #5140.
const EXIT_ASSESSMENT_FAILED: i32 = 3;
